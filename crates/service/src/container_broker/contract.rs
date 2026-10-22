//! Private provider handshake and immutable lease registration.

use open_compute_core::{
    ErrorCode, PlatformError, config::validate_container_socket, containers::ContainerResources,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct SnapshotCaptureRequest {
    pub(super) start_identity: String,
    pub(super) max_bytes: u64,
}

/// Independently operated Broker settings; ordinary daemon startup never launches it.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerConfig {
    /// Private mode-0600 endpoint shared with the daemon and workerd.
    pub socket: PathBuf,
    /// Operator-installed Docker socket, exposed only to this process.
    pub engine_socket: PathBuf,
    /// Unix user permitted to connect to the Broker.
    pub peer_uid: u32,
    /// Fixed, pre-materialized egress interceptor image configuration digest.
    pub sidecar_image: Option<String>,
    /// Pre-materialized local volume containing the immutable exec helper.
    pub exec_helper_volume: String,
    /// SHA-256 of the helper bytes in both the fixed sidecar image and helper volume.
    pub exec_helper_sha256: String,
    /// Pre-materialized Linux image containing `/bin/sh` and `dd`, used for resource qualification.
    pub qualification_image: String,
    /// Maximum retained lease identities, including uncertain operations and fenced tombstones.
    pub max_leases: u32,
    /// Provider-wide live reservation limit, independent of retained tombstones.
    pub max_instances: u32,
    /// Maximum simultaneous HTTP requests, including streaming exec sessions.
    pub max_requests: u32,
    /// Host Docker published-port address reachable from the shared Broker/workerd namespace.
    pub relay_host: std::net::IpAddr,
    /// Private operator-owned directory for bounded, unnamed import spools.
    pub spool_dir: PathBuf,
    /// Private persistent directory for exclusive ownership and uncertain-write fences.
    pub state_dir: PathBuf,
    /// Maximum materialization archive bytes, including decompressed layer transport.
    pub max_image_bytes: u64,
    /// Maximum total bytes in either direction of one exec stream.
    pub max_stream_bytes: u64,
    /// Application resource ceiling.
    pub resources: ContainerResources,
}

impl BrokerConfig {
    /// Reject unsafe endpoints and unbounded operator policy before engine I/O.
    pub fn validate(&self) -> Result<(), PlatformError> {
        validate_container_socket(&self.socket)?;
        validate_container_socket(&self.engine_socket)?;
        if self.socket == self.engine_socket
            || self
                .sidecar_image
                .as_deref()
                .is_some_and(|image| !image_id(image))
            || !image_id(&self.qualification_image)
            || !super::helper::volume_name(&self.exec_helper_volume)
            || !opaque(&self.exec_helper_sha256)
            || !(1..=100_000).contains(&self.max_leases)
            || !(1..=1_024).contains(&self.max_instances)
            || self.max_instances > self.max_leases
            || !(1..=1_024).contains(&self.max_requests)
            || !self.spool_dir.is_absolute()
            || !self.state_dir.is_absolute()
            || self.max_image_bytes == 0
            || self.max_image_bytes > 1 << 40
            || self.max_stream_bytes == 0
            || self.max_stream_bytes > 1 << 40
            || self.resources.pids > i32::MAX as u32
        {
            return Err(invalid());
        }
        self.relay_capacity()?;
        self.resources.validate(self.resources)
    }

    pub(super) fn relay_capacity(&self) -> Result<usize, PlatformError> {
        // Profile v2 maintains sixteen idle reverse callback channels per live lease.
        usize::try_from(self.max_instances)
            .ok()
            .and_then(|instances| instances.checked_mul(16))
            .and_then(|idle| {
                usize::try_from(self.max_requests)
                    .ok()
                    .and_then(|active| idle.checked_add(active))
            })
            .ok_or_else(invalid)
    }
}

/// Secret-free result of active provider qualification.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderContract {
    /// Version of the private schema and framing contract.
    pub version: u32,
    /// Engine product; no host endpoint or credential is included.
    pub runtime: String,
    /// Exact engine version inspected at qualification.
    pub engine_version: String,
    /// Docker architecture, normalized to OCI `amd64` or `arm64`.
    pub architecture: String,
    /// Inspected isolation implementation.
    pub isolation: String,
    /// Whether CPU, memory, disk, PID and file limits passed active checks.
    pub qualified: bool,
    /// Whether pause/export/unpause, process continuity and volatile `/run` exclusion passed.
    pub snapshots: bool,
    /// Immutable interceptor image configuration digest.
    pub sidecar_image: Option<String>,
    /// Immutable exec helper bytes verified against the fixed sidecar image and mounted volume.
    pub exec_helper_sha256: String,
    /// Qualified per-application upper bounds.
    pub resources: ContainerResources,
    /// Provider-wide admission ceiling, including unknown owned identities.
    pub max_instances: u32,
    /// Maximum offline materialization archive bytes.
    pub max_image_bytes: u64,
    /// Maximum bidirectional bytes per exec stream.
    pub max_stream_bytes: u64,
}

impl ProviderContract {
    /// Current required lease schema, including trusted scheduling policy.
    pub const VERSION: u32 = 3;

    /// Digest the canonical contract bytes without private host topology.
    pub fn sha256(&self) -> Result<String, PlatformError> {
        let bytes = serde_json::to_vec(self).map_err(|_| invalid())?;
        Ok(hex::encode(Sha256::digest(bytes)))
    }
}

/// Ocd-authorized immutable execution policy, registered before native create/start.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrokerLease {
    /// Opaque, non-reusable engine key from persisted authority.
    pub lease_key: String,
    /// Stable object owner hashed from the persisted instance, namespace and object identity.
    pub owner_key: String,
    /// Immutable startup options decrypted and validated by the daemon authority.
    pub startup: super::startup::BrokerStartOptions,
    /// Persisted application policy; native requests cannot select process capabilities.
    pub scheduling_policy: open_compute_storage::containers::ContainerSchedulingPolicy,
    /// Registered nonce authenticated in engine labels; process identity is observed separately.
    pub start_identity: String,
    /// Immutable OCI manifest identity.
    pub manifest_sha256: String,
    /// Verified Docker image configuration ID; tags and registries are prohibited.
    pub image: String,
    /// Ordered rootfs diff IDs verified against the admitted image graph.
    pub diff_ids: Vec<String>,
    /// Digest of the provider contract admitted into the application target.
    pub provider_contract_sha256: String,
    /// Effective limits injected by the Broker, independent of native request JSON.
    pub resources: ContainerResources,
    /// Owning application identity from the persisted namespace authority.
    pub application_id: open_compute_core::ResourceId,
    /// Current application capacity ceiling, checked again at Broker admission.
    pub max_instances: u32,
}

impl BrokerLease {
    /// Bind a registration to a qualified contract and its exact resource limits.
    pub fn validate(&self, contract: &ProviderContract) -> Result<(), PlatformError> {
        if contract.version != ProviderContract::VERSION
            || !contract.qualified
            || !opaque(&contract.exec_helper_sha256)
            || !opaque(&self.lease_key)
            || !opaque(&self.owner_key)
            || !opaque(&self.start_identity)
            || !opaque(&self.manifest_sha256)
            || !image_id(&self.image)
            || self.diff_ids.len() > 128
            || self.diff_ids.iter().any(|id| !image_id(id))
            || self.provider_contract_sha256 != contract.sha256()?
            || self.max_instances == 0
            || self.max_instances > contract.max_instances
        {
            return Err(invalid());
        }
        self.startup.validate()?;
        self.resources.validate(contract.resources)
    }
}

/// Closed image proof supplied by the daemon's verified immutable image authority.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ImageProof {
    pub image: String,
    pub diff_ids: Vec<String>,
}

impl ImageProof {
    pub(super) fn validate(&self) -> Result<(), PlatformError> {
        if !image_id(&self.image)
            || self.diff_ids.len() > 128
            || self.diff_ids.iter().any(|id| !image_id(id))
        {
            return Err(invalid());
        }
        Ok(())
    }
}

/// Actual engine size of an image whose immutable configuration and ordered rootfs were verified.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ImageSize {
    pub size: u64,
}

/// Verified provider observation; absence alone never proves cleanup or terminal state.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderLeaseObservation {
    /// Hash of the verified Docker ID and process start timestamp, empty before observation.
    pub start_identity: String,
    /// Verified engine configuration image digest.
    pub image: String,
    /// Admitted immutable OCI graph identity.
    pub manifest_sha256: String,
    /// Matched provider contract.
    pub provider_contract_sha256: String,
    /// Positive observation of the same main process running.
    pub running: bool,
    /// Main process termination proved by the explicit termination protocol.
    pub terminated: bool,
    /// Both application and proxy removals positively confirmed.
    pub cleanup_complete: bool,
    /// Non-reusable lease tombstone prevents subsequent mutations.
    pub late_operations_fenced: bool,
}

pub(super) fn opaque(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(super) fn image_id(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(opaque)
}

pub(super) fn invalid() -> PlatformError {
    PlatformError::new(
        ErrorCode::ConfigInvalid,
        "Container provider contract rejected",
    )
}

pub(super) fn process_identity(
    value: &serde_json::Value,
) -> Result<(String, String), PlatformError> {
    Ok((
        value
            .get("Id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| opaque(id))
            .ok_or_else(invalid)?
            .to_owned(),
        value
            .pointer("/State/StartedAt")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty() && value.len() <= 64)
            .ok_or_else(invalid)?
            .to_owned(),
    ))
}

pub(super) fn process_start_identity(key: &str, identity: &(String, String)) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"dev.open-compute.container.process.v1\0");
    hasher.update(key.as_bytes());
    hasher.update(identity.0.as_bytes());
    hasher.update(identity.1.as_bytes());
    hex::encode(hasher.finalize())
}
