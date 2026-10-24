use super::*;

/// Operator ceilings shared by pending, ready and retiring Container snapshots.
#[derive(Clone, Copy, Debug)]
pub struct ContainerSnapshotLimits {
    /// Maximum retained snapshot records in this instance.
    pub max_snapshots: u32,
    /// Maximum retained or reserved artifact bytes in this instance.
    pub max_bytes: u64,
}

/// Exact source execution fingerprint frozen before capture I/O.
#[derive(Clone, Eq, PartialEq)]
pub struct ContainerSnapshotSource {
    /// Source object and lease generation.
    pub identity: ContainerLeaseIdentity,
    /// Opaque provider key, never a tenant-visible snapshot identifier.
    pub opaque_engine_key: String,
    /// Positively observed provider process fingerprint.
    pub start_identity: String,
}

impl std::fmt::Debug for ContainerSnapshotSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ContainerSnapshotSource")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

/// Input for an application-owned snapshot reservation.
#[derive(Clone, Debug)]
pub struct NewContainerSnapshot {
    /// Canonical public snapshot UUID.
    pub id: ResourceId,
    /// Exact running source execution.
    pub source: ContainerSnapshotSource,
    /// Optional public descriptive name.
    pub name: Option<String>,
    /// Captured directory, or `None` for a complete root filesystem snapshot.
    pub directory: Option<open_compute_core::containers::ContainerDirectoryPath>,
    /// Complete artifact-byte reservation, retained while publication is unknown.
    pub reserved_bytes: u64,
}

/// Persistent capture and retirement state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContainerSnapshotState {
    /// Capture or immutable publication has not been positively completed.
    Pending,
    /// A verified immutable artifact can be selected by an authorized restore.
    Ready,
    /// New restores are fenced; bytes remain charged until positive absence.
    Retiring,
}

/// Durable application ACL, capture fingerprint and artifact retention authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContainerSnapshot {
    /// Canonical public snapshot UUID.
    pub id: ResourceId,
    /// Owning installation.
    pub instance_id: InstanceId,
    /// Immutable application identity; the source DO is not the restore ACL.
    pub application_id: ResourceId,
    /// Exact source execution for reconciliation and capture fencing.
    pub source: ContainerSnapshotSource,
    /// Effective source image lineage.
    pub source_image_sha256: [u8; 32],
    /// Derived immutable execution image, known before artifact publication.
    pub image_sha256: Option<[u8; 32]>,
    /// Immutable artifact digest, persisted before the first PUT.
    pub artifact_sha256: Option<[u8; 32]>,
    /// Exact artifact bytes, persisted before the first PUT.
    pub artifact_size: Option<u64>,
    /// Public size: provider image bytes for rootfs capture, tar bytes for a directory capture.
    pub image_size: Option<u64>,
    /// Optional public descriptive name.
    pub name: Option<String>,
    /// Immutable capture path; directory snapshots cannot become startup images.
    pub directory: Option<open_compute_core::containers::ContainerDirectoryPath>,
    /// Capture and retirement state.
    pub state: ContainerSnapshotState,
    /// Original byte reservation; pending captures keep its complete charge.
    pub reserved_bytes: u64,
    /// Reservation timestamp.
    pub created_at_ms: i64,
    /// External PUT was initiated; an unpublished attempt cannot prove absence after late retries.
    pub put_attempted: bool,
    /// Positive immutable publication timestamp.
    pub captured_at_ms: Option<i64>,
    /// Exclusive deadline for admitting another restore.
    pub expires_at_ms: i64,
    /// First positive Running observation of the latest successful restore lease.
    pub last_restored_at_ms: Option<i64>,
}
