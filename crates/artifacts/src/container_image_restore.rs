//! Restore a complete immutable OCI archive without extracting files onto the host.

use super::*;
use crate::ArtifactRef;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::os::unix::fs::FileExt;

/// Verify the artifact bytes, closed OCI archive layout, and complete image graph.
/// Reads use file offsets and do not change the caller's file cursor.
pub fn verify_container_image_archive(
    file: &File,
    artifact: &ArtifactRef,
    expected_manifest: [u8; 32],
    architecture: &str,
    limits: ContainerImageLimits,
) -> Result<VerifiedContainerImage, PlatformError> {
    verify_archive(file, artifact, expected_manifest, architecture, limits).map(|(image, _)| image)
}

/// Read the original image configuration after verifying the complete sealed artifact.
/// Runtime environment variables and engine inspection data are not image configuration.
pub fn read_container_image_configuration(
    file: &File,
    artifact: &ArtifactRef,
    expected_manifest: [u8; 32],
    architecture: &str,
    limits: ContainerImageLimits,
) -> Result<(VerifiedContainerImage, Vec<u8>), PlatformError> {
    let (image, members) = verify_archive(file, artifact, expected_manifest, architecture, limits)?;
    let configuration = read_member(
        file,
        &members,
        &blob_path(&image.config.sha256),
        limits.json_bytes,
    )?;
    Ok((image, configuration))
}

/// Stream a verified OCI artifact as a Docker save transport for an offline engine import.
/// The conversion does not change the persisted OCI authority or extract host files.
pub fn write_container_docker_archive<W: io::Write>(
    file: &File,
    artifact: &ArtifactRef,
    expected_manifest: [u8; 32],
    architecture: &str,
    limits: ContainerImageLimits,
    output: W,
) -> Result<W, PlatformError> {
    let (image, members) = verify_archive(file, artifact, expected_manifest, architecture, limits)?;
    archive::write_docker_archive(
        &image,
        |blob| {
            let (offset, size) = members
                .get(&blob_path(&blob.sha256))
                .ok_or_else(integrity_error)?;
            if *size != blob.bytes {
                return Err(integrity_error());
            }
            Ok(FileReader::new(file, *offset)?.take(*size))
        },
        output,
    )
}

type ArchiveMembers = BTreeMap<String, (u64, u64)>;

fn verify_archive(
    file: &File,
    artifact: &ArtifactRef,
    expected_manifest: [u8; 32],
    architecture: &str,
    limits: ContainerImageLimits,
) -> Result<(VerifiedContainerImage, ArchiveMembers), PlatformError> {
    let max_entries = limits.layers.checked_add(4).ok_or_else(integrity_error)?;
    let max_bytes = limits.archive_bytes()?;
    if artifact.size() == 0 || artifact.size() > max_bytes {
        return Err(integrity_error());
    }
    let inspected =
        verify_container_archive(FileReader::new(file, 0)?, artifact.size(), max_entries)?;
    if inspected.sha256 != *artifact.sha256_bytes() || inspected.bytes != artifact.size() {
        return Err(integrity_error());
    }
    let mut archive = tar::Archive::new(FileReader::new(file, 0)?);
    let mut members = BTreeMap::new();
    // The complete archive was already hashed and validated; index headers without rereading blobs.
    for entry in archive.entries_with_seek().map_err(|_| integrity_error())? {
        let entry = entry.map_err(|_| integrity_error())?;
        let path = std::str::from_utf8(&entry.path_bytes())
            .map_err(|_| integrity_error())?
            .to_owned();
        if !entry.header().entry_type().is_file()
            || !valid_member(&path)
            || members
                .insert(path, (entry.raw_file_position(), entry.size()))
                .is_some()
        {
            return Err(integrity_error());
        }
    }
    let layout: Layout = serde_json::from_slice(&read_member(
        file,
        &members,
        "oci-layout",
        limits.json_bytes,
    )?)
    .map_err(|_| integrity_error())?;
    let index: Index = serde_json::from_slice(&read_member(
        file,
        &members,
        "index.json",
        limits.json_bytes,
    )?)
    .map_err(|_| integrity_error())?;
    if layout.image_layout_version != "1.0.0"
        || index.schema_version != 2
        || index.manifests.len() != 1
    {
        return Err(integrity_error());
    }
    let descriptor = &index.manifests[0];
    if digest(&descriptor.digest)? != expected_manifest
        || descriptor.platform.os != "linux"
        || descriptor.platform.architecture != architecture
    {
        return Err(integrity_error());
    }
    let manifest_path = blob_path(&expected_manifest);
    let manifest_bytes = read_member(file, &members, &manifest_path, limits.json_bytes)?;
    let manifest: Manifest =
        serde_json::from_slice(&manifest_bytes).map_err(|_| integrity_error())?;
    if descriptor.size != manifest_bytes.len() as u64
        || descriptor.media_type != manifest.media_type
    {
        return Err(integrity_error());
    }
    let verified = verify_container_image(
        &manifest_bytes,
        expected_manifest,
        architecture,
        limits,
        |blob| {
            let (offset, size) = members
                .get(&blob_path(&blob.sha256))
                .ok_or_else(integrity_error)?;
            if *size != blob.bytes {
                return Err(integrity_error());
            }
            Ok(FileReader::new(file, *offset)?.take(*size))
        },
    )?;
    let mut required = BTreeSet::from([
        "oci-layout".to_owned(),
        "index.json".to_owned(),
        manifest_path,
        blob_path(&verified.config.sha256),
    ]);
    required.extend(verified.layers.iter().map(|blob| blob_path(&blob.sha256)));
    if members.keys().cloned().collect::<BTreeSet<_>>() != required {
        return Err(integrity_error());
    }
    Ok((verified, members))
}

fn blob_path(sha256: &[u8; 32]) -> String {
    format!("blobs/sha256/{}", hex::encode(sha256))
}

fn valid_member(path: &str) -> bool {
    matches!(path, "oci-layout" | "index.json")
        || path
            .strip_prefix("blobs/sha256/")
            .is_some_and(|hex| digest(&format!("sha256:{hex}")).is_ok())
}

fn read_member(
    file: &File,
    members: &BTreeMap<String, (u64, u64)>,
    path: &str,
    limit: u64,
) -> Result<Vec<u8>, PlatformError> {
    let (offset, size) = members.get(path).ok_or_else(integrity_error)?;
    if *size == 0 || *size > limit {
        return Err(integrity_error());
    }
    let mut bytes = Vec::new();
    FileReader::new(file, *offset)?
        .take(*size)
        .read_to_end(&mut bytes)
        .map_err(|_| integrity_error())?;
    if bytes.len() as u64 != *size {
        return Err(integrity_error());
    }
    Ok(bytes)
}

pub(super) struct FileReader {
    file: File,
    offset: u64,
}
impl FileReader {
    pub(super) fn new(file: &File, offset: u64) -> Result<Self, PlatformError> {
        Ok(Self {
            file: file.try_clone().map_err(|_| integrity_error())?,
            offset,
        })
    }
}
impl Read for FileReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.file.read_at(buffer, self.offset)?;
        self.offset = self
            .offset
            .checked_add(count as u64)
            .ok_or_else(|| io::Error::other("archive offset exceeds bounds"))?;
        Ok(count)
    }
}

impl Seek for FileReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let offset = match from {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::Current(offset) => self.offset.checked_add_signed(offset),
            SeekFrom::End(offset) => self.file.metadata()?.len().checked_add_signed(offset),
        }
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "archive offset exceeds bounds")
        })?;
        self.offset = offset;
        Ok(offset)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Layout {
    image_layout_version: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Index {
    schema_version: u32,
    manifests: Vec<IndexManifest>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IndexManifest {
    media_type: String,
    digest: String,
    size: u64,
    platform: Platform,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Platform {
    os: String,
    architecture: String,
}
