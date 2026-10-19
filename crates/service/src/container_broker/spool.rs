//! Bounded staging before any engine import mutation.

use super::contract::invalid;
use axum::body::Body;
use futures::StreamExt;
use open_compute_core::PlatformError;
use std::{
    io::{Seek, SeekFrom},
    os::unix::fs::PermissionsExt,
    path::Path,
};
use tokio::io::AsyncWriteExt;

pub(super) async fn stage(
    body: Body,
    directory: &Path,
    maximum: u64,
) -> Result<(tokio::fs::File, u64), PlatformError> {
    stage_verified(body, directory, maximum, 4_096).await
}

pub(super) async fn stage_rootfs(
    body: Body,
    directory: &Path,
    maximum: u64,
) -> Result<(tokio::fs::File, u64), PlatformError> {
    // The existing admitted image graph permits this many entries across all layers.
    stage_verified(body, directory, maximum, 100_000).await
}

async fn stage_verified(
    body: Body,
    directory: &Path,
    maximum: u64,
    entries: usize,
) -> Result<(tokio::fs::File, u64), PlatformError> {
    super::socket::validate_directory(directory, rustix::process::getuid().as_raw())?;
    let file = tempfile::tempfile_in(directory).map_err(|_| invalid())?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|_| invalid())?;
    // Batch transport fragments before dispatching file writes to the blocking pool.
    let mut file = tokio::io::BufWriter::with_capacity(256 * 1024, tokio::fs::File::from_std(file));
    let mut stream = body.into_data_stream();
    let mut bytes = 0_u64;
    while let Some(part) = tokio::time::timeout(std::time::Duration::from_secs(30), stream.next())
        .await
        .map_err(|_| invalid())?
    {
        let part = part.map_err(|_| invalid())?;
        if part.len() as u64 > maximum.saturating_sub(bytes) {
            return Err(invalid());
        }
        bytes += part.len() as u64;
        file.write_all(&part).await.map_err(|_| invalid())?;
    }
    file.flush().await.map_err(|_| invalid())?;
    let file = file.into_inner().into_std().await;
    let file = tokio::task::spawn_blocking(move || {
        let mut file = file;
        file.seek(SeekFrom::Start(0)).map_err(|_| invalid())?;
        open_compute_artifacts::verify_container_archive(&mut file, maximum, entries)?;
        file.seek(SeekFrom::Start(0)).map_err(|_| invalid())?;
        Ok::<_, PlatformError>(file)
    })
    .await
    .map_err(|_| invalid())??;
    Ok((tokio::fs::File::from_std(file), bytes))
}
