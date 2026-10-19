//! Bounded OCI graph admission. No image content is extracted onto the host.

use crate::error::integrity_error;
use flate2::read::MultiGzDecoder;
use open_compute_core::PlatformError;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::{self, Read};

/// Operator bounds applied before reading or decompressing untrusted image content.
#[derive(Clone, Copy, Debug)]
pub struct ContainerImageLimits {
    /// Maximum manifest and configuration JSON size individually.
    pub json_bytes: u64,
    /// Maximum total compressed layer size.
    pub compressed_bytes: u64,
    /// Maximum total uncompressed layer archive size, including headers and padding.
    pub unpacked_bytes: u64,
    /// Maximum layer count.
    pub layers: usize,
    /// Maximum archive entries across all layers.
    pub entries: usize,
}

impl ContainerImageLimits {
    /// Conservative staging budget for the complete OCI archive, including tar framing.
    pub fn archive_bytes(self) -> Result<u64, PlatformError> {
        let entries = self.layers.checked_add(4).ok_or_else(integrity_error)? as u64;
        self.json_bytes
            .checked_mul(4)
            .and_then(|n| n.checked_add(self.compressed_bytes))
            .and_then(|n| n.checked_add(entries.checked_mul(1024)?))
            .and_then(|n| n.checked_add(1024))
            .ok_or_else(integrity_error)
    }
}

/// One content-addressed blob required by the admitted manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OciBlob {
    /// SHA-256 of the exact blob bytes.
    pub sha256: [u8; 32],
    /// Exact descriptor size.
    pub bytes: u64,
}

/// Complete verified image graph. Construction requires successful validation of every blob.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedContainerImage {
    manifest_sha256: [u8; 32],
    manifest_bytes: u64,
    manifest_media_type: String,
    config: OciBlob,
    architecture: String,
    layers: Vec<OciBlob>,
    diff_ids: Vec<[u8; 32]>,
    unpacked_bytes: u64,
}

impl VerifiedContainerImage {
    /// Digest of the immutable admitted manifest.
    #[must_use]
    pub const fn manifest_sha256(&self) -> &[u8; 32] {
        &self.manifest_sha256
    }
    /// Verified image configuration descriptor.
    #[must_use]
    pub const fn config(&self) -> &OciBlob {
        &self.config
    }
    /// Verified Linux architecture.
    #[must_use]
    pub fn architecture(&self) -> &str {
        &self.architecture
    }
    /// Ordered, verified layer descriptors.
    #[must_use]
    pub fn layers(&self) -> &[OciBlob] {
        &self.layers
    }
    /// Ordered digests of the verified uncompressed layer archives.
    #[must_use]
    pub fn diff_ids(&self) -> &[[u8; 32]] {
        &self.diff_ids
    }
    /// Conservative upper bound from all decompressed layer archives.
    #[must_use]
    pub const fn unpacked_bytes(&self) -> u64 {
        self.unpacked_bytes
    }
}

/// Verify a single-platform OCI/Docker image and its entire graph without registry access.
/// `read_blob` must return immutable artifact content selected by the supplied descriptor.
/// Only Linux amd64/arm64 images and tar/gzip layers are admitted.
pub fn verify_container_image<R: Read>(
    manifest_bytes: &[u8],
    expected_sha256: [u8; 32],
    architecture: &str,
    limits: ContainerImageLimits,
    mut read_blob: impl FnMut(&OciBlob) -> Result<R, PlatformError>,
) -> Result<VerifiedContainerImage, PlatformError> {
    if limits.json_bytes == 0
        || limits.compressed_bytes == 0
        || limits.unpacked_bytes == 0
        || limits.layers == 0
        || limits.entries == 0
        || manifest_bytes.is_empty()
        || manifest_bytes.len() as u64 > limits.json_bytes
        || !matches!(architecture, "amd64" | "arm64")
        || <[u8; 32]>::from(Sha256::digest(manifest_bytes)) != expected_sha256
    {
        return Err(integrity_error());
    }
    let manifest: Manifest =
        serde_json::from_slice(manifest_bytes).map_err(|_| integrity_error())?;
    if manifest.schema_version != 2
        || !matches!(
            manifest.media_type.as_str(),
            "application/vnd.oci.image.manifest.v1+json"
                | "application/vnd.docker.distribution.manifest.v2+json"
        )
        || manifest.artifact_type.is_some()
        || manifest.subject.is_some()
        || manifest.layers.len() > limits.layers
        || !matches!(
            manifest.config.media_type.as_str(),
            "application/vnd.oci.image.config.v1+json"
                | "application/vnd.docker.container.image.v1+json"
        )
    {
        return Err(integrity_error());
    }
    let config_blob = manifest.config.blob(limits.json_bytes)?;
    let mut config_reader = CheckedReader::new(
        read_blob(&config_blob).map_err(|_| integrity_error())?,
        config_blob.bytes,
    );
    let mut config_bytes = Vec::new();
    config_reader
        .read_to_end(&mut config_bytes)
        .map_err(|_| integrity_error())?;
    config_reader.verify(&config_blob.sha256)?;
    let config: Config = serde_json::from_slice(&config_bytes).map_err(|_| integrity_error())?;
    if config.os != "linux"
        || config.architecture != architecture
        || config.rootfs.kind != "layers"
        || config.rootfs.diff_ids.len() != manifest.layers.len()
    {
        return Err(integrity_error());
    }
    let diff_ids = config
        .rootfs
        .diff_ids
        .iter()
        .map(|id| digest(id))
        .collect::<Result<Vec<_>, _>>()?;
    let mut blobs = Vec::with_capacity(manifest.layers.len());
    let mut compressed_bytes = 0_u64;
    let mut unpacked_bytes = 0_u64;
    let mut entries_left = limits.entries;
    for (descriptor, diff_id) in manifest.layers.iter().zip(&diff_ids) {
        let gzip = match descriptor.media_type.as_str() {
            "application/vnd.oci.image.layer.v1.tar" => false,
            "application/vnd.oci.image.layer.v1.tar+gzip"
            | "application/vnd.docker.image.rootfs.diff.tar.gzip" => true,
            _ => return Err(integrity_error()),
        };
        let blob = descriptor.blob(limits.compressed_bytes)?;
        compressed_bytes = compressed_bytes
            .checked_add(blob.bytes)
            .filter(|n| *n <= limits.compressed_bytes)
            .ok_or_else(integrity_error)?;
        let mut compressed =
            CheckedReader::new(read_blob(&blob).map_err(|_| integrity_error())?, blob.bytes);
        let remaining = limits.unpacked_bytes - unpacked_bytes;
        let count = if gzip {
            verify_layer(
                MultiGzDecoder::new(&mut compressed),
                *diff_id,
                remaining,
                &mut entries_left,
            )?
        } else {
            verify_layer(&mut compressed, *diff_id, remaining, &mut entries_left)?
        };
        compressed.verify(&blob.sha256)?;
        unpacked_bytes += count;
        blobs.push(blob);
    }
    Ok(VerifiedContainerImage {
        manifest_sha256: expected_sha256,
        manifest_bytes: manifest_bytes.len() as u64,
        manifest_media_type: manifest.media_type,
        config: config_blob,
        architecture: config.architecture,
        layers: blobs,
        diff_ids,
        unpacked_bytes,
    })
}

#[path = "container_image_archive.rs"]
mod archive;
pub use archive::write_container_image_archive;

#[path = "container_image_restore.rs"]
mod restore;
pub use restore::{
    read_container_image_configuration, verify_container_image_archive,
    write_container_docker_archive,
};

#[path = "container_image_snapshot.rs"]
mod snapshot;
pub use snapshot::write_container_snapshot_image;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    schema_version: u32,
    media_type: String,
    config: Descriptor,
    layers: Vec<Descriptor>,
    artifact_type: Option<String>,
    subject: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Descriptor {
    media_type: String,
    digest: String,
    size: u64,
    // Remote or embedded descriptors cannot bypass the artifact store.
    urls: Option<serde_json::Value>,
    data: Option<serde_json::Value>,
}

impl Descriptor {
    fn blob(&self, limit: u64) -> Result<OciBlob, PlatformError> {
        if self.size == 0 || self.size > limit || self.urls.is_some() || self.data.is_some() {
            return Err(integrity_error());
        }
        Ok(OciBlob {
            sha256: digest(&self.digest)?,
            bytes: self.size,
        })
    }
}

#[derive(Deserialize)]
struct Config {
    architecture: String,
    os: String,
    rootfs: RootFs,
}
#[derive(Deserialize)]
struct RootFs {
    #[serde(rename = "type")]
    kind: String,
    diff_ids: Vec<String>,
}

fn digest(value: &str) -> Result<[u8; 32], PlatformError> {
    let hex = value
        .strip_prefix("sha256:")
        .filter(|hex| {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        .ok_or_else(integrity_error)?;
    let mut result = [0; 32];
    hex::decode_to_slice(hex, &mut result).map_err(|_| integrity_error())?;
    Ok(result)
}

fn verify_layer(
    reader: impl Read,
    diff_id: [u8; 32],
    limit: u64,
    entries_left: &mut usize,
) -> Result<u64, PlatformError> {
    let inspected = inspect_archive(reader, limit, entries_left)?;
    if inspected.sha256 != diff_id {
        return Err(integrity_error());
    }
    Ok(inspected.bytes)
}

/// Validate a bounded container tar stream without extracting it onto the host.
///
/// Checks paths, links, entry types, extension metadata, checksums and archive termination.
/// The returned digest describes the exact archive bytes, including zero padding.
pub fn verify_container_archive(
    reader: impl Read,
    max_bytes: u64,
    max_entries: usize,
) -> Result<OciBlob, PlatformError> {
    if max_bytes == 0 || max_entries == 0 {
        return Err(integrity_error());
    }
    inspect_archive(reader, max_bytes, &mut { max_entries })
}

fn inspect_archive(
    reader: impl Read,
    limit: u64,
    entries_left: &mut usize,
) -> Result<OciBlob, PlatformError> {
    let mut archive = tar::Archive::new(ArchiveReader::new(
        io::BufReader::with_capacity(256 * 1024, CheckedReader::new(reader, limit)),
        *entries_left,
    ));
    let mut names = HashSet::new();
    for entry in archive.entries().map_err(|_| integrity_error())? {
        let mut entry = entry.map_err(|_| integrity_error())?;
        let name = entry.path_bytes();
        let normalized = name
            .split(|b| *b == b'/')
            .filter(|part| !part.is_empty() && *part != b".")
            .collect::<Vec<_>>()
            .join(&b'/');
        if !valid_path(&name) || !names.insert(normalized) {
            return Err(integrity_error());
        }
        let kind = entry.header().entry_type();
        if !matches!(
            kind,
            tar::EntryType::Regular
                | tar::EntryType::Directory
                | tar::EntryType::Symlink
                | tar::EntryType::Link
        ) {
            return Err(integrity_error());
        }
        if kind == tar::EntryType::Link
            && !entry
                .link_name_bytes()
                .is_some_and(|name| valid_path(&name))
        {
            return Err(integrity_error());
        }
        if kind == tar::EntryType::Symlink
            && !entry
                .link_name_bytes()
                .is_some_and(|name| !name.is_empty() && name.len() <= 4096 && !name.contains(&0))
        {
            return Err(integrity_error());
        }
        io::copy(&mut entry, &mut io::sink()).map_err(|_| integrity_error())?;
    }
    let mut reader = archive.into_inner();
    // A second archive or non-zero payload after the end marker is ambiguous to extractors.
    let mut tail = [0; 8192];
    loop {
        let size = reader.read(&mut tail).map_err(|_| integrity_error())?;
        if size == 0 {
            break;
        }
        if tail[..size].iter().any(|byte| *byte != 0) {
            return Err(integrity_error());
        }
    }
    *entries_left = reader.headers_left;
    let reader = reader.inner.into_inner();
    if reader.count == 0 {
        return Err(integrity_error());
    }
    Ok(OciBlob {
        sha256: reader.hash.finalize().into(),
        bytes: reader.count,
    })
}

fn valid_path(path: &[u8]) -> bool {
    !path.is_empty()
        && path.len() <= 4096
        && !path.starts_with(b"/")
        && !path.contains(&0)
        && !path.split(|b| *b == b'/').any(|part| part == b"..")
}

// Inspect raw extension headers before tar allocates their content. The tar crate still owns
// checksum, PAX/GNU resolution, entry parsing, and padding semantics.
struct ArchiveReader<R> {
    inner: R,
    header: [u8; 512],
    position: usize,
    payload_left: u64,
    headers_left: usize,
}

impl<R: Read> ArchiveReader<R> {
    fn new(inner: R, headers_left: usize) -> Self {
        Self {
            inner,
            header: [0; 512],
            position: 512,
            payload_left: 0,
            headers_left,
        }
    }
}

impl<R: Read> Read for ArchiveReader<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.position == 512 && self.payload_left != 0 {
            let size = usize::try_from(self.payload_left)
                .unwrap_or(usize::MAX)
                .min(output.len());
            let count = self.inner.read(&mut output[..size])?;
            self.payload_left -= count as u64;
            return Ok(count);
        }
        if self.position == 512 {
            let mut read = 0;
            while read < 512 {
                let size = self.inner.read(&mut self.header[read..])?;
                if size == 0 {
                    if read == 0 {
                        return Ok(0);
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "truncated image archive",
                    ));
                }
                read += size;
            }
            if self.header.iter().any(|byte| *byte != 0) {
                self.headers_left = self
                    .headers_left
                    .checked_sub(1)
                    .ok_or_else(|| io::Error::other("image archive entry bound exceeded"))?;
                let header = tar::Header::from_byte_slice(&self.header);
                let bytes = header.entry_size()?;
                if matches!(
                    header.entry_type(),
                    tar::EntryType::GNULongName
                        | tar::EntryType::GNULongLink
                        | tar::EntryType::XHeader
                        | tar::EntryType::XGlobalHeader
                ) && bytes > 16384
                {
                    return Err(io::Error::other("image archive metadata bound exceeded"));
                }
                self.payload_left = bytes
                    .checked_add(511)
                    .map(|n| n / 512 * 512)
                    .ok_or_else(|| io::Error::other("invalid image archive size"))?;
            }
            self.position = 0;
        }
        let count = (512 - self.position).min(output.len());
        output[..count].copy_from_slice(&self.header[self.position..self.position + count]);
        self.position += count;
        Ok(count)
    }
}

struct CheckedReader<R> {
    inner: R,
    hash: Sha256,
    count: u64,
    limit: u64,
}
impl<R: Read> CheckedReader<R> {
    fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            hash: Sha256::new(),
            count: 0,
            limit,
        }
    }
    fn verify(self, expected: &[u8; 32]) -> Result<(), PlatformError> {
        if self.count != self.limit || self.hash.finalize().as_slice() != expected {
            return Err(integrity_error());
        }
        Ok(())
    }
}
impl<R: Read> Read for CheckedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let allowed = usize::try_from(self.limit.saturating_sub(self.count).saturating_add(1))
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        let count = self.inner.read(&mut buffer[..allowed])?;
        self.count = self
            .count
            .checked_add(count as u64)
            .filter(|n| *n <= self.limit)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "image exceeds admission bound")
            })?;
        self.hash.update(&buffer[..count]);
        Ok(count)
    }
}

#[cfg(test)]
#[path = "container_image_tests.rs"]
mod tests;
