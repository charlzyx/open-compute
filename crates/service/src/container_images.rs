//! Admission of complete OCI artifact graphs; all I/O precedes the SQLite commit.

use crate::ContainerService;
use open_compute_artifacts::{
    ArtifactCache, ArtifactRef, ArtifactStore, ContainerImageLimits, OciBlob, PinnedArtifact,
};
use open_compute_core::{ErrorCode, PlatformError};
use open_compute_storage::{
    PlatformStorage,
    containers::{ContainerAuthority, ContainerImage},
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek};

pub(crate) struct ContainerImageUpload<'a> {
    pub(crate) manifest: &'a [u8],
    pub(crate) expected: [u8; 32],
    pub(crate) architecture: &'a str,
    pub(crate) blobs: Vec<ArtifactRef>,
    pub(crate) limits: ContainerImageLimits,
}

impl ContainerService {
    pub(crate) async fn admit_image(
        &self,
        upload: ContainerImageUpload<'_>,
    ) -> Result<ContainerImage, PlatformError> {
        admit_image(&self.storage, &self.artifacts, &self.cache, upload).await
    }
}

async fn admit_image(
    storage: &PlatformStorage,
    artifacts: &ArtifactStore,
    cache: &ArtifactCache,
    upload: ContainerImageUpload<'_>,
) -> Result<ContainerImage, PlatformError> {
    let ContainerImageUpload {
        manifest,
        expected,
        architecture,
        blobs,
        limits,
    } = upload;
    let _gc = artifacts.reserve_version_artifact().await;
    if manifest.len() as u64 > limits.json_bytes {
        return Err(invalid());
    }
    let budget = limits.archive_bytes()?;
    let _disk = storage.reserve_mutation(budget)?;
    if blobs.len() > limits.layers.checked_add(1).ok_or_else(invalid)? {
        return Err(invalid());
    }
    let mut pins = BTreeMap::new();
    for blob in blobs {
        let pin = cache.acquire(artifacts, &blob).await?;
        if pins
            .insert(*blob.sha256_bytes(), (blob.size(), pin))
            .is_some()
        {
            return Err(invalid());
        }
    }
    let output = tempfile::NamedTempFile::new_in(storage.data_dir().prepare_tmp_dir()?)
        .map_err(|_| invalid())?;
    let manifest = manifest.to_vec();
    let architecture = architecture.to_owned();
    let (image, output, size, digest) = tokio::task::spawn_blocking(move || {
        let image = open_compute_artifacts::verify_container_image(
            &manifest,
            expected,
            &architecture,
            limits,
            |blob| reader(&pins, blob),
        )?;
        let required = std::iter::once(image.config())
            .chain(image.layers())
            .map(|blob| blob.sha256)
            .collect::<BTreeSet<_>>();
        if pins.keys().copied().collect::<BTreeSet<_>>() != required {
            return Err(invalid());
        }
        let mut output = open_compute_artifacts::write_container_image_archive(
            &image,
            &manifest,
            |blob| reader(&pins, blob),
            output,
        )?;
        output.as_file().sync_all().map_err(|_| invalid())?;
        let size = output.as_file().metadata().map_err(|_| invalid())?.len();
        if size > budget {
            return Err(invalid());
        }
        output.rewind().map_err(|_| invalid())?;
        let mut sha256 = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = output.read(&mut buffer).map_err(|_| invalid())?;
            if count == 0 {
                break;
            }
            sha256.update(&buffer[..count]);
        }
        Ok((image, output, size, hex::encode(sha256.finalize())))
    })
    .await
    .map_err(|_| invalid())??;
    let artifact = artifacts
        .put_verified_file(output.path(), &digest, size)
        .await?;
    let metadata = ContainerImage {
        manifest_sha256: expected,
        config_sha256: image.config().sha256,
        diff_ids: image.diff_ids().to_vec(),
        architecture: image.architecture().to_owned(),
        compressed_bytes: image.layers().iter().try_fold(0_u64, |sum, blob| {
            sum.checked_add(blob.bytes).ok_or_else(invalid)
        })?,
        unpacked_bytes: image.unpacked_bytes(),
        artifact_sha256: *artifact.sha256_bytes(),
        artifact_size: artifact.size(),
        created_at_ms: open_compute_core::wall_time_ms(),
    };
    ContainerAuthority::new(storage.db()).admit_image(storage.identity().instance_id, &metadata)?;
    ContainerAuthority::new(storage.db()).image(storage.identity().instance_id, &expected)
}

fn reader(
    pins: &BTreeMap<[u8; 32], (u64, PinnedArtifact)>,
    blob: &OciBlob,
) -> Result<std::fs::File, PlatformError> {
    let (size, pin) = pins.get(&blob.sha256).ok_or_else(invalid)?;
    if *size != blob.bytes {
        return Err(invalid());
    }
    let mut file = pin.file().try_clone().map_err(|_| invalid())?;
    file.rewind().map_err(|_| invalid())?;
    Ok(file)
}

fn invalid() -> PlatformError {
    PlatformError::new(
        ErrorCode::ObjectStorageIntegrityError,
        "Container image graph admission failed",
    )
}

#[cfg(test)]
#[path = "container_images_tests.rs"]
mod tests;
