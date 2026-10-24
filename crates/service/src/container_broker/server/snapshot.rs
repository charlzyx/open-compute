//! One fenced, process-bound root filesystem capture; no general archive or pause API.

use super::super::contract::SnapshotCaptureRequest;
use super::*;
use tokio::io::AsyncReadExt as _;

pub(super) async fn capture(
    state: &BrokerState,
    request: Request,
    path: &str,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<Response<Body>, PlatformError> {
    if !state.contract.snapshots {
        return Err(invalid());
    }
    let key = path
        .strip_prefix("/open-compute/v1/leases/")
        .and_then(|path| path.strip_suffix("/snapshot"))
        .filter(|key| opaque(key))
        .ok_or_else(invalid)?;
    let bytes = read_body(request.into_body(), 32_768).await?;
    let request: SnapshotCaptureRequest = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if !opaque(&request.start_identity)
        || request.max_bytes == 0
        || request.max_bytes > state.config.max_image_bytes
    {
        return Err(invalid());
    }
    let entry = state
        .leases
        .lock()
        .await
        .get(key)
        .cloned()
        .ok_or_else(invalid)?;
    let mut entry = entry.lock().await;
    if entry.fenced || entry.cleaned || state.journal.contains(key)? {
        return Err(invalid());
    }
    let (identity, paused) = inspect(state, &entry, key).await?;
    if paused
        || entry.process_identity.as_ref() != Some(&identity)
        || process_start_identity(key, &identity) != request.start_identity
    {
        return Err(invalid());
    }
    // A drop at any subsequent await leaves both the durable marker and in-memory fence.
    state.journal.begin(key)?;
    entry.fenced = true;
    let mut fence = CaptureFence {
        entry: &mut entry,
        armed: true,
    };
    let (file, size) = tokio::time::timeout(std::time::Duration::from_secs(180), async {
        let id = &identity.0;
        let (file, size) = match &request.directory {
            Some(directory) => {
                state
                    .engine
                    .capture_directory(id, directory, &state.config.spool_dir, request.max_bytes)
                    .await?
            }
            None => {
                state
                    .engine
                    .capture_rootfs(id, &state.config.spool_dir, request.max_bytes)
                    .await?
            }
        };
        let (after, paused) = inspect(state, fence.entry, id).await?;
        if after != identity || paused {
            return Err(invalid());
        }
        state.journal.clear(key)?;
        Ok::<_, PlatformError>((file, size))
    })
    .await
    .map_err(|_| invalid())??;
    fence.entry.fenced = false;
    fence.armed = false;
    Response::builder()
        .status(200)
        .header("Content-Type", "application/x-tar")
        .header("Content-Length", size)
        .body(archive_body(file, size, permit))
        .map_err(|_| invalid())
}

struct CaptureFence<'a> {
    entry: &'a mut LeaseEntry,
    armed: bool,
}

impl Drop for CaptureFence<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.entry.generation_cancel.cancel();
            self.entry.relay = None;
            self.entry.credentials = None;
            self.entry.generation_sha256 = None;
        }
    }
}

async fn inspect(
    state: &BrokerState,
    entry: &LeaseEntry,
    lookup: &str,
) -> Result<((String, String), bool), PlatformError> {
    let (status, value) = state
        .engine
        .json(
            Method::GET,
            &format!("/containers/{lookup}/json"),
            Value::Null,
        )
        .await?;
    if status != 200 || value.pointer("/State/Running").and_then(Value::as_bool) != Some(true) {
        return Err(invalid());
    }
    verify_inspection(
        state,
        &value,
        &entry.lease.lease_key,
        &entry.lease,
        &entry.startup,
    )
    .await?;
    let paused = value
        .pointer("/State/Paused")
        .and_then(Value::as_bool)
        .ok_or_else(invalid)?;
    Ok((process_identity(&value)?, paused))
}

fn archive_body(
    file: tokio::fs::File,
    size: u64,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Body {
    Body::from_stream(futures::stream::try_unfold(
        (file, size, permit),
        |(mut file, left, permit)| async move {
            let mut buffer = vec![0; left.clamp(1, 65_536) as usize];
            let count = file.read(&mut buffer).await?;
            if left == 0 {
                if count != 0 {
                    return Err(std::io::Error::other("archive size changed"));
                }
                return Ok(None);
            }
            if count == 0 {
                return Err(std::io::Error::other("archive truncated"));
            }
            buffer.truncate(count);
            Ok(Some((buffer, (file, left - count as u64, permit))))
        },
    ))
}
