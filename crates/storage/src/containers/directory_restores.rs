//! Directory restore plans are reserved with the execution that owns their writable clones.

use super::management::read_image;
use super::*;
use open_compute_core::containers::ContainerDirectoryPath;
use std::collections::BTreeSet;

/// One immutable directory restore choice, independent of a Container boot image.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContainerDirectoryRestore {
    /// Application-owned backup, or none for a new empty directory.
    #[serde(deserialize_with = "serde::Deserialize::deserialize")]
    pub snapshot_id: Option<ResourceId>,
    /// Canonical non-root destination inside the Container, never a host path.
    pub mount_point: ContainerDirectoryPath,
}

impl ContainerAuthority<'_> {
    /// Read only the backups held by an exact persisted lease, including cleanup after deletion.
    /// This metadata read grants no current route, start, or tenant control authority.
    pub fn directory_lease_inputs(
        &self,
        instance: InstanceId,
        identity: ContainerLeaseIdentity,
        engine_key: &str,
    ) -> Result<Vec<(ContainerDirectoryRestore, Option<ContainerImage>)>, PlatformError> {
        self.db.with_read(|conn| {
            require_instance(conn, instance)?;
            let lease = leases::read_lease(conn, identity)?;
            if lease.opaque_engine_key != engine_key {
                return Err(invariant());
            }
            let base = read_image(conn, &lease.effective_image_sha256)?;
            lease
                .directory_snapshots
                .into_iter()
                .map(|entry| {
                    let image = entry
                        .snapshot_id
                        .map(|id| {
                            let image = snapshots::retained_directory_image(
                                conn,
                                instance,
                                lease.application_id,
                                id,
                            )?;
                            if image.architecture != base.architecture {
                                return Err(invariant());
                            }
                            Ok(image)
                        })
                        .transpose()?;
                    Ok((entry, image))
                })
                .collect()
        })
    }

    /// Resolve the frozen directory inputs for the current exact execution.
    /// A live lease retains its backups after expiry; this does not authorize a new restore.
    pub fn directory_restore_inputs(
        &self,
        authority: &ContainerObjectAuthority,
        engine_key: &str,
    ) -> Result<Vec<(ContainerDirectoryRestore, Option<ContainerImage>)>, PlatformError> {
        self.db.with_read(|conn| {
            let lease = leases::read_authorized_live(conn, authority)?.ok_or_else(not_found)?;
            if lease.opaque_engine_key != engine_key || lease.stop_requested_at_ms.is_some() {
                return Err(invariant());
            }
            let base = read_image(conn, &lease.effective_image_sha256)?;
            lease
                .directory_snapshots
                .into_iter()
                .map(|entry| {
                    let image = entry
                        .snapshot_id
                        .map(|id| {
                            let (snapshot, image) =
                                snapshots::retained_snapshot(conn, authority, id)?;
                            if snapshot.directory.is_none()
                                || image.architecture != base.architecture
                            {
                                return Err(invariant());
                            }
                            Ok(image)
                        })
                        .transpose()?;
                    Ok((entry, image))
                })
                .collect()
        })
    }
}

pub(super) fn validate_plan(plan: &[ContainerDirectoryRestore]) -> Result<(), PlatformError> {
    let mut paths = BTreeSet::new();
    for entry in plan {
        if entry.mount_point.as_str() == "/" || !paths.insert(entry.mount_point.as_str()) {
            return Err(invariant());
        }
    }
    Ok(())
}

pub(super) fn validate_execution(
    conn: &Connection,
    authority: &ContainerObjectAuthority,
    execution: &ContainerExecutionSpec,
    now_ms: i64,
) -> Result<(), PlatformError> {
    if execution.directory_snapshots.is_empty() {
        return Ok(());
    }
    let image = read_image(conn, &execution.image_sha256)?;
    for entry in &execution.directory_snapshots {
        if let Some(id) = entry.snapshot_id {
            let (snapshot, backup) = snapshots::restore_snapshot(conn, authority, id, now_ms)?;
            if snapshot.directory.is_none() || backup.architecture != image.architecture {
                return Err(invariant());
            }
        }
    }
    Ok(())
}
