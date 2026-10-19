//! Build a flattened snapshot from a bounded root filesystem and sealed image configuration.

use super::*;
use restore::FileReader;
use serde_json::{Value, json};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Cursor, Seek, Write};

struct GzipLayer(BufReader<flate2::read::GzEncoder<CheckedReader<FileReader>>>);

impl GzipLayer {
    fn new(rootfs: &File, bytes: u64) -> Result<Self, PlatformError> {
        Ok(Self(BufReader::with_capacity(
            65_536,
            flate2::read::GzEncoder::new(
                CheckedReader::new(FileReader::new(rootfs, 0)?, bytes),
                flate2::Compression::fast(),
            ),
        )))
    }
}

impl Read for GzipLayer {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        // Fix compressor read sizes so hashing and archive consumers receive identical gzip bytes.
        let available = self.0.fill_buf()?;
        let count = available.len().min(buffer.len());
        buffer[..count].copy_from_slice(&available[..count]);
        self.0.consume(count);
        Ok(count)
    }
}

/// Write a deterministic gzip layer in an OCI snapshot without extracting host files.
///
/// The configuration must match the admitted source image. Only the root filesystem and
/// layer history change; runtime startup options and platform credentials are never copied.
/// The caller provides an empty private scratch file for the bounded compressed layer.
/// A failure can leave partial output, which the caller must not publish.
pub fn write_container_snapshot_image<W: Write>(
    source: &VerifiedContainerImage,
    configuration: &[u8],
    rootfs: &File,
    mut compressed_layer: File,
    limits: ContainerImageLimits,
    output: W,
) -> Result<(VerifiedContainerImage, W), PlatformError> {
    if limits.layers == 0
        || configuration.len() as u64 != source.config.bytes
        || configuration.len() as u64 > limits.json_bytes
        || <[u8; 32]>::from(Sha256::digest(configuration)) != source.config.sha256
        || compressed_layer
            .metadata()
            .map_err(|_| integrity_error())?
            .len()
            != 0
    {
        return Err(integrity_error());
    }
    let rootfs_layer = verify_container_archive(
        FileReader::new(rootfs, 0)?,
        limits.unpacked_bytes,
        limits.entries,
    )?;
    let mut compressed = GzipLayer::new(rootfs, rootfs_layer.bytes)?;
    compressed_layer.rewind().map_err(|_| integrity_error())?;
    let mut staged = BufWriter::with_capacity(256 * 1024, compressed_layer);
    let mut digest = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; 65_536];
    loop {
        let count = compressed
            .read(&mut buffer)
            .map_err(|_| integrity_error())?;
        if count == 0 {
            break;
        }
        if count as u64 > limits.compressed_bytes.saturating_sub(bytes) {
            return Err(integrity_error());
        }
        bytes += count as u64;
        digest.update(&buffer[..count]);
        staged
            .write_all(&buffer[..count])
            .map_err(|_| integrity_error())?;
    }
    compressed
        .0
        .into_inner()
        .into_inner()
        .verify(&rootfs_layer.sha256)?;
    let compressed_layer = staged.into_inner().map_err(|_| integrity_error())?;
    let layer = OciBlob {
        sha256: digest.finalize().into(),
        bytes,
    };
    let mut configuration: Value =
        serde_json::from_slice(configuration).map_err(|_| integrity_error())?;
    let object = configuration.as_object_mut().ok_or_else(integrity_error)?;
    object.remove("history");
    object.insert(
        "rootfs".to_owned(),
        json!({"type":"layers","diff_ids":[format!("sha256:{}", hex::encode(rootfs_layer.sha256))]}),
    );
    let configuration = serde_json::to_vec(&configuration).map_err(|_| integrity_error())?;
    if configuration.len() as u64 > limits.json_bytes {
        return Err(integrity_error());
    }
    let config = OciBlob {
        sha256: Sha256::digest(&configuration).into(),
        bytes: configuration.len() as u64,
    };
    let manifest = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": format!("sha256:{}", hex::encode(config.sha256)),
            "size": config.bytes
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": format!("sha256:{}", hex::encode(layer.sha256)),
            "size": layer.bytes
        }]
    }))
    .map_err(|_| integrity_error())?;
    let read_blob = |blob: &OciBlob| -> Result<Box<dyn Read + '_>, PlatformError> {
        if blob == &config {
            Ok(Box::new(Cursor::new(configuration.as_slice())))
        } else if blob == &layer {
            Ok(Box::new(FileReader::new(&compressed_layer, 0)?))
        } else {
            Err(integrity_error())
        }
    };
    if manifest.len() as u64 > limits.json_bytes {
        return Err(integrity_error());
    }
    // The sealed configuration and validated tar already prove this generated one-layer graph.
    let image = VerifiedContainerImage {
        manifest_sha256: Sha256::digest(&manifest).into(),
        manifest_bytes: manifest.len() as u64,
        manifest_media_type: "application/vnd.oci.image.manifest.v1+json".to_owned(),
        config: config.clone(),
        architecture: source.architecture().to_owned(),
        layers: vec![layer.clone()],
        diff_ids: vec![rootfs_layer.sha256],
        unpacked_bytes: rootfs_layer.bytes,
    };
    let output = write_container_image_archive(&image, &manifest, read_blob, output)?;
    // Detect rootfs changes during publication without compressing the same bytes again.
    let mut rootfs = CheckedReader::new(FileReader::new(rootfs, 0)?, rootfs_layer.bytes);
    io::copy(&mut rootfs, &mut io::sink()).map_err(|_| integrity_error())?;
    rootfs.verify(&rootfs_layer.sha256)?;
    Ok((image, output))
}
