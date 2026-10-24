use super::*;

/// Verified immutable OCI image metadata. Artifact bytes are validated before insertion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContainerImage {
    /// OCI manifest digest.
    pub manifest_sha256: [u8; 32],
    /// OCI image configuration digest.
    pub config_sha256: [u8; 32],
    /// Ordered verified uncompressed layer digests; at most 128 layers.
    pub diff_ids: Vec<[u8; 32]>,
    /// OCI architecture: amd64 or arm64.
    pub architecture: String,
    /// Total admitted compressed bytes.
    pub compressed_bytes: u64,
    /// Verified unpacked bytes.
    pub unpacked_bytes: u64,
    /// Content-addressed artifact containing the manifest, configuration, and layers.
    pub artifact_sha256: [u8; 32],
    /// Exact size of the complete offline image archive.
    pub artifact_size: u64,
    /// Admission timestamp.
    pub created_at_ms: i64,
}

/// Public metadata updates; omitted fields preserve their current authoritative values.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ContainerApplicationMetadataPatch {
    /// New application capacity, when explicitly requested.
    pub max_instances: Option<u32>,
    /// New log collection request intent, when explicitly requested.
    pub observability_logs: Option<bool>,
}

/// Validated effective image, provider, resource, and egress configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContainerTargetSpec {
    /// Worker Version that admitted this target.
    pub worker_version_id: VersionId,
    /// Admitted default image; absent for Durable Object-managed applications.
    pub image_sha256: Option<[u8; 32]>,
    /// Qualified provider contract digest.
    pub provider_contract_sha256: [u8; 32],
    /// Normalized provider limits.
    pub resources: ContainerResources,
    /// General outbound policy.
    pub enable_internet: bool,
}

/// One admitted application target; publication never rewrites an existing target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContainerTarget {
    /// Stable application identity.
    pub application_id: ResourceId,
    /// Monotonic target generation.
    pub generation: u64,
    /// Immutable effective configuration.
    pub spec: ContainerTargetSpec,
    /// Target admission timestamp.
    pub created_at_ms: i64,
}

/// Native facet authority resolved from the current DO dispatch and immutable declaration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContainerObjectAuthority {
    /// Owning instance.
    pub instance_id: InstanceId,
    /// Declared application.
    pub application_id: ResourceId,
    /// Stable namespace.
    pub namespace_resource_id: ResourceId,
    /// Stable object identity.
    pub object_id: DurableObjectId,
    /// Object deletion/recreation generation.
    pub object_generation: u64,
    /// Current Worker route generation, separate from the lease generation.
    pub route_generation: u64,
    /// Digest of the immutable Container class declaration.
    pub descriptor_sha256: [u8; 32],
}

/// Immutable identity used for every asynchronous provider operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContainerLeaseIdentity {
    /// Stable namespace.
    pub namespace_resource_id: ResourceId,
    /// Stable object identity.
    pub object_id: DurableObjectId,
    /// Object deletion/recreation generation.
    pub object_generation: u64,
    /// Monotonic execution generation.
    pub lease_generation: u64,
}

/// One durable execution reservation, including uncertain or terminated executions.
#[derive(Clone, Eq, PartialEq)]
pub struct ContainerLease {
    /// Fences asynchronous operations to this execution.
    pub identity: ContainerLeaseIdentity,
    /// Application that owns the capacity reservation.
    pub application_id: ResourceId,
    /// Effective immutable target; it may precede the application's current target.
    pub target_generation: u64,
    /// Immutable admitted image selected for this execution.
    pub effective_image_sha256: [u8; 32],
    /// Immutable normalized resource limits selected for this execution.
    pub effective_resources: ContainerResources,
    /// Immutable general outbound policy selected for this execution.
    pub enable_internet: bool,
    /// Validated startup identity and overrides; secret-bearing values are never stored here.
    pub startup: ContainerLeaseStartup,
    /// Immutable directory clones selected for this execution, including empty mounts.
    pub directory_snapshots: Vec<ContainerDirectoryRestore>,
    /// AEAD envelope bound to this instance, object owner and exact provider key.
    pub startup_envelope: Vec<u8>,
    /// Opaque provider identity; never expose it to tenants or public status.
    pub opaque_engine_key: String,
    /// Provider start identity once observed; never replace it on reconnect.
    pub start_identity: Option<String>,
    /// Current observation state.
    pub state: ContainerLeaseState,
    /// Capacity reservation timestamp.
    pub created_at_ms: i64,
    /// Last authority transition timestamp.
    pub updated_at_ms: i64,
    /// Confirmed terminal timestamp.
    pub stopped_at_ms: Option<i64>,
    /// Durable termination intent; provider uncertainty must not restore a running grant.
    pub stop_requested_at_ms: Option<i64>,
}

/// Positive provider evidence required before releasing capacity and object identity.
#[derive(Clone, Copy, Debug)]
pub struct ContainerTermination<'a> {
    /// Provider key verified against the immutable lease.
    pub opaque_engine_key: &'a str,
    /// Observed start identity, or none when the provider proves no start occurred.
    pub start_identity: Option<&'a str>,
    /// Provider confirms that the main process is absent or exited.
    pub terminated: bool,
    /// Application, sidecar, and owned ephemeral resources are cleaned up.
    pub cleanup_complete: bool,
    /// Provider rejects all subsequent create/start operations for this lease.
    pub late_operations_fenced: bool,
}

/// Verified provider observation of exactly one immutable execution.
#[derive(Clone, Copy, Debug)]
pub struct ContainerRunningObservation<'a> {
    /// Provider key verified against the execution reservation.
    pub opaque_engine_key: &'a str,
    /// Provider start identity.
    pub start_identity: &'a str,
    /// Effective image observed by the provider.
    pub image_sha256: &'a [u8; 32],
    /// Provider contract under which this process was created.
    pub provider_contract_sha256: &'a [u8; 32],
}

/// Request identity checked against the active immutable Container declaration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VersionContainerIdentity {
    /// Same-Worker SQLite namespace identity.
    pub namespace_resource_id: ResourceId,
    /// Application name in the active declaration.
    pub application_name: String,
    /// Digest binding the complete declaration, including its named images.
    pub descriptor_sha256: [u8; 32],
}

/// Immutable association between a Worker Version and a Container DO namespace.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VersionContainerDeclaration {
    /// Same-Worker SQLite namespace identity.
    pub namespace_resource_id: ResourceId,
    /// Application name emitted by the deployment producer.
    pub application_name: String,
    /// Immutable named image references admitted with this Worker Version.
    pub images: std::collections::BTreeMap<String, String>,
    /// Digest of the canonical namespace, application, and named images declaration.
    pub descriptor_sha256: [u8; 32],
}

/// Authoritative application scheduling model from the official deployment producer.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerSchedulingPolicy {
    /// Platform-managed immutable image target and rollout.
    Default,
    /// Image and resources selected by each Durable Object execution.
    DurableObject,
}
impl ContainerSchedulingPolicy {
    /// Canonical persisted and public spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::DurableObject => "durable_object",
        }
    }
}

/// Persistent application settings; image and resources belong to immutable targets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContainerApplication {
    /// Stable application identity.
    pub id: ResourceId,
    /// Same-Worker SQLite namespace.
    pub namespace_resource_id: ResourceId,
    /// Deployment application name.
    pub name: String,
    /// Canonical scheduling model; Durable Object-managed applications have no default image.
    pub scheduling_policy: ContainerSchedulingPolicy,
    /// Capacity ceiling including uncertain executions.
    pub max_instances: u32,
    /// Published immutable target, if materialized.
    pub target_generation: Option<u64>,
    /// Admission stays paused until provider reconciliation succeeds.
    pub admission_paused: bool,
    /// Creation timestamp.
    pub created_at_ms: i64,
}

/// Validated rollout request matched to the live immutable target inside one transaction.
#[derive(Clone, Copy, Debug)]
pub struct ContainerRolloutCreate<'a> {
    /// New rollout control-plane identity.
    pub id: ResourceId,
    /// Closed public configuration serialized by the request producer.
    pub target_configuration: &'a [u8],
    /// Strictly increasing cumulative percentages, ending at one hundred.
    pub steps: &'a [u32],
    /// Optional descriptive metadata in the exact step order.
    pub details: Option<&'a ContainerRolloutDetails>,
}

/// Durable progressive rollout, bound to an immutable application target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContainerRollout {
    /// Stable control-plane identity.
    pub id: ResourceId,
    /// Owning application.
    pub application_id: ResourceId,
    /// Target used for replacement starts.
    pub target_generation: u64,
    /// Immutable configuration generation observed when this rollout was created.
    pub current_generation: u64,
    /// Validated descriptive metadata preserved from the control request.
    pub details: ContainerRolloutDetails,
    /// Strictly increasing cumulative percentages, ending at one hundred.
    pub steps: Vec<u32>,
    /// Zero-based active step, or steps length after completion.
    pub current_step: usize,
    /// Persistent running/completed/failed/cancelled state.
    pub state: String,
    /// Creation timestamp.
    pub created_at_ms: i64,
    /// Last transition timestamp.
    pub updated_at_ms: i64,
}

/// Closed, non-secret startup metadata frozen when capacity is reserved.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContainerLeaseStartup {
    /// Canonical public immutable image reference, or empty for a snapshot restore.
    pub image_reference: String,
    /// Application-owned snapshot selected for this immutable restore lease.
    #[serde(deserialize_with = "serde::Deserialize::deserialize")]
    pub snapshot_id: Option<ResourceId>,
    /// Optional native named resource profile.
    pub instance_profile: Option<open_compute_core::containers::ContainerInstanceProfile>,
    /// Preserve an explicit immutable image override during application rollout.
    pub image_override: bool,
    /// Preserve explicitly requested resource limits during application rollout.
    pub resources_override: bool,
}

impl ContainerLeaseStartup {
    pub(super) fn validate_image_reference(
        &self,
        image_sha256: &[u8; 32],
    ) -> Result<(), PlatformError> {
        if self.snapshot_id.is_some() {
            return if self.image_reference.is_empty() {
                Ok(())
            } else {
                Err(invariant())
            };
        }
        let (reference, digest) = self
            .image_reference
            .rsplit_once("@sha256:")
            .ok_or_else(invariant)?;
        if reference.is_empty()
            || reference.len() > 512
            || !reference.is_ascii()
            || reference
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
            || digest != hex::encode(image_sha256)
        {
            return Err(invariant());
        }
        Ok(())
    }
}

/// Validated startup overrides; immutable application target identity remains separate.
#[derive(Clone, Eq, PartialEq)]
pub struct ContainerExecutionSpec {
    /// An admitted immutable OCI image digest.
    pub image_sha256: [u8; 32],
    /// Resources bounded by the application and operator policies.
    pub resources: ContainerResources,
    /// General outbound policy, bounded by application policy.
    pub enable_internet: bool,
    /// Closed startup metadata.
    pub startup: ContainerLeaseStartup,
    /// Application-authorized directory clone plan, frozen atomically with capacity.
    pub directory_snapshots: Vec<ContainerDirectoryRestore>,
    /// Encrypted runtime options; plaintext never enters SQLite.
    pub startup_envelope: Vec<u8>,
}

/// Descriptive rollout metadata, separate from execution control and identity.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContainerRolloutDetails {
    /// Optional rollout description.
    pub description: Option<String>,
    /// Optional descriptions in the exact persisted step order.
    pub step_descriptions: Vec<Option<String>>,
}

impl std::fmt::Debug for ContainerLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ContainerLease")
            .field("identity", &self.identity)
            .field("application_id", &self.application_id)
            .field("target_generation", &self.target_generation)
            .field("effective_resources", &self.effective_resources)
            .field("enable_internet", &self.enable_internet)
            .field("startup", &self.startup)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ContainerExecutionSpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ContainerExecutionSpec")
            .field("image_sha256", &self.image_sha256)
            .field("resources", &self.resources)
            .field("enable_internet", &self.enable_internet)
            .field("startup", &self.startup)
            .finish_non_exhaustive()
    }
}
