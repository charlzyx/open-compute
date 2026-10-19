use super::*;
use std::io::Write;

/// Write a deterministic OCI image-layout archive for offline provider materialization.
///
/// The private verification result determines every member. Blob bytes are rehashed while
/// writing, so changing a staged blob after admission cannot produce a publishable archive.
/// A failure may leave partial output; callers must publish only after successful completion.
pub fn write_container_image_archive<W: Write, R: Read>(
    image: &VerifiedContainerImage,
    manifest: &[u8],
    mut read_blob: impl FnMut(&OciBlob) -> Result<R, PlatformError>,
    output: W,
) -> Result<W, PlatformError> {
    if manifest.len() as u64 != image.manifest_bytes
        || <[u8; 32]>::from(Sha256::digest(manifest)) != image.manifest_sha256
    {
        return Err(integrity_error());
    }
    let index = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "manifests": [{
            "mediaType": image.manifest_media_type,
            "digest": format!("sha256:{}", hex::encode(image.manifest_sha256)),
            "size": image.manifest_bytes,
            "platform": { "os": "linux", "architecture": image.architecture }
        }]
    }))
    .map_err(|_| integrity_error())?;
    let mut archive = tar::Builder::new(output);
    append(
        &mut archive,
        "oci-layout",
        br#"{"imageLayoutVersion":"1.0.0"}"#,
    )?;
    append(&mut archive, "index.json", &index)?;
    append(&mut archive, &blob_path(&image.manifest_sha256), manifest)?;
    let mut written = HashSet::from([image.manifest_sha256]);
    for blob in std::iter::once(&image.config).chain(&image.layers) {
        if !written.insert(blob.sha256) {
            continue;
        }
        let mut reader =
            CheckedReader::new(read_blob(blob).map_err(|_| integrity_error())?, blob.bytes);
        let mut header = header(blob.bytes);
        archive
            .append_data(&mut header, blob_path(&blob.sha256), &mut reader)
            .map_err(|_| integrity_error())?;
        // append_data reads exactly the header's size; require EOF as well.
        let mut extra = [0_u8; 1];
        reader.read(&mut extra).map_err(|_| integrity_error())?;
        reader.verify(&blob.sha256)?;
    }
    archive.into_inner().map_err(|_| integrity_error())
}

fn append<W: Write>(
    archive: &mut tar::Builder<W>,
    name: &str,
    bytes: &[u8],
) -> Result<(), PlatformError> {
    archive
        .append_data(&mut header(bytes.len() as u64), name, bytes)
        .map_err(|_| integrity_error())
}

pub(super) fn write_docker_archive<W: Write, R: Read>(
    image: &VerifiedContainerImage,
    mut read_blob: impl FnMut(&OciBlob) -> Result<R, PlatformError>,
    output: W,
) -> Result<W, PlatformError> {
    let config_path = format!("{}.json", hex::encode(image.config.sha256));
    let layer_paths = image
        .diff_ids
        .iter()
        .map(|id| format!("{}/layer.tar", hex::encode(id)))
        .collect::<Vec<_>>();
    let docker_manifest = serde_json::to_vec(&serde_json::json!([{
        "Config": config_path, "RepoTags": [], "Layers": layer_paths
    }]))
    .map_err(|_| integrity_error())?;
    let mut archive = tar::Builder::new(output);
    append(&mut archive, "manifest.json", &docker_manifest)?;
    let mut config = CheckedReader::new(
        read_blob(&image.config).map_err(|_| integrity_error())?,
        image.config.bytes,
    );
    archive
        .append_data(&mut header(image.config.bytes), config_path, &mut config)
        .map_err(|_| integrity_error())?;
    let mut extra = [0_u8; 1];
    config.read(&mut extra).map_err(|_| integrity_error())?;
    config.verify(&image.config.sha256)?;
    let mut written = HashSet::new();
    for (index, blob) in image.layers.iter().enumerate() {
        if !written.insert(image.diff_ids[index]) {
            continue;
        }
        // The complete OCI graph already verified each compressed blob and unpacked DiffID.
        // Docker decodes embedded layer streams; retain the original transport bytes.
        let mut reader =
            CheckedReader::new(read_blob(blob).map_err(|_| integrity_error())?, blob.bytes);
        archive
            .append_data(&mut header(blob.bytes), &layer_paths[index], &mut reader)
            .map_err(|_| integrity_error())?;
        let mut extra = [0_u8; 1];
        reader.read(&mut extra).map_err(|_| integrity_error())?;
        reader.verify(&blob.sha256)?;
    }
    archive.into_inner().map_err(|_| integrity_error())
}

fn blob_path(sha256: &[u8; 32]) -> String {
    format!("blobs/sha256/{}", hex::encode(sha256))
}

fn header(bytes: u64) -> tar::Header {
    let mut header = tar::Header::new_ustar();
    header.set_size(bytes);
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_entry_type(tar::EntryType::Regular);
    header
}
