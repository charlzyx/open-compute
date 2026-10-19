//! Recover abandoned captures and release charges only after verified object absence.

use super::*;
use open_compute_storage::containers::ContainerSnapshotState;

impl ContainerService {
    pub(crate) async fn reconcile_snapshots(&self) -> Result<(), PlatformError> {
        let repo = ContainerAuthority::new(self.storage.db());
        let instance = self.instance_id();
        for snapshot in repo.snapshots(instance)? {
            if snapshot.state != ContainerSnapshotState::Pending {
                continue;
            }
            let active = self
                .snapshot_captures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !active.contains(&snapshot.id) {
                repo.retire_snapshot(instance, snapshot.id)?;
            }
        }
        repo.retire_snapshots(instance, open_compute_core::wall_time_ms())?;
        for snapshot in repo.snapshots(instance)? {
            if snapshot.state == ContainerSnapshotState::Retiring
                && snapshot.captured_at_ms.is_none()
            {
                if self
                    .snapshot_captures
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .contains(&snapshot.id)
                {
                    continue;
                }
                let source = repo.lease(instance, snapshot.source.identity)?;
                if source.opaque_engine_key != snapshot.source.opaque_engine_key {
                    return Err(unavailable());
                }
                if source.state != ContainerLeaseState::Stopped {
                    repo.begin_stop(
                        source.identity,
                        &source.opaque_engine_key,
                        open_compute_core::wall_time_ms(),
                    )?;
                    let stopping = repo.lease(instance, source.identity)?;
                    self.finish_maintenance_stop(&stopping, Some(0)).await?;
                }
            }
        }
        // This fence covers the reference snapshot, object deletion and final quota release.
        let _gc = self.artifacts.fence_version_gc().await;
        let references =
            open_compute_storage::worker_repository::WorkerRepository::new(self.storage.db())
                .referenced_artifacts()?;
        for snapshot in repo.snapshots(instance)? {
            if snapshot.state != ContainerSnapshotState::Retiring {
                continue;
            }
            if snapshot.captured_at_ms.is_none() {
                let source = repo.lease(instance, snapshot.source.identity)?;
                if source.state != ContainerLeaseState::Stopped {
                    continue;
                }
            }
            match (snapshot.artifact_sha256, snapshot.artifact_size) {
                (Some(sha256), Some(size)) => {
                    if references.contains(&(sha256, size)) {
                        continue;
                    }
                    let artifact = ArtifactRef::new(
                        open_compute_artifacts::ARTIFACT_KEY_VERSION,
                        &hex::encode(sha256),
                        size,
                    )?;
                    self.artifacts.delete_unreferenced(&artifact).await?;
                    if !self.artifacts.is_absent(&artifact).await? {
                        return Err(unavailable());
                    }
                }
                (None, None) => {}
                _ => return Err(unavailable()),
            }
            if snapshot.captured_at_ms.is_some() || !snapshot.put_attempted {
                repo.finish_snapshot_retirement(instance, snapshot.id, true)?;
            }
        }
        Ok(())
    }
}
