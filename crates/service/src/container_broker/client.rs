//! Daemon-side admission through the operator-owned Broker.

use super::{
    contract::{BrokerLease, ProviderContract, ProviderLeaseObservation, invalid},
    engine::{Engine, bounded_response},
};
use axum::http::Method;
use open_compute_core::{PlatformError, config::ContainersConfig, containers::ContainerResources};
use serde_json::Value;

#[path = "client_snapshot.rs"]
mod snapshot;

#[path = "client_directory.rs"]
mod directory;

/// Qualified provider handle. Only validated preflight can construct this type.
#[derive(Clone)]
pub struct QualifiedContainerProvider {
    engine: Engine,
    contract: ProviderContract,
    contract_sha256: String,
    max_instances: u32,
    resources: ContainerResources,
    sidecar_image: String,
    max_snapshots: u32,
    max_snapshot_bytes: u64,
}

impl std::fmt::Debug for QualifiedContainerProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QualifiedContainerProvider")
            .field("contract_sha256", &self.contract_sha256)
            .field("max_instances", &self.max_instances)
            .finish_non_exhaustive()
    }
}

impl QualifiedContainerProvider {
    /// Verify socket security, active Broker qualification and the pinned contract.
    pub async fn preflight(config: &ContainersConfig) -> Result<Self, PlatformError> {
        config.validate()?;
        super::socket::validate_socket(&config.broker_socket, rustix::process::getuid().as_raw())?;
        let engine = Engine::new(&config.broker_socket, "/open-compute/v1/contract").await?;
        let (status, value) = engine
            .json(Method::GET, "/open-compute/v1/contract", Value::Null)
            .await?;
        if status != 200 {
            return Err(invalid());
        }
        let contract: ProviderContract = serde_json::from_value(value).map_err(|_| invalid())?;
        let contract_sha256 = contract.sha256()?;
        if contract.version != ProviderContract::VERSION
            || contract.runtime != "docker"
            || !contract.qualified
            || !super::contract::opaque(&contract.exec_helper_sha256)
            || contract_sha256 != config.provider_contract_sha256
            || config.max_instances > contract.max_instances
        {
            return Err(invalid());
        }
        config.resources.validate(contract.resources)?;
        let sidecar_image = contract
            .sidecar_image
            .as_ref()
            .filter(|image| super::contract::image_id(image))
            .ok_or_else(invalid)?
            .clone();
        Ok(Self {
            engine,
            contract,
            contract_sha256,
            max_instances: config.max_instances,
            resources: config.resources,
            sidecar_image,
            max_snapshots: config.max_snapshots,
            max_snapshot_bytes: config.max_snapshot_bytes,
        })
    }

    /// Maximum retained snapshots admitted by this daemon's operator configuration.
    #[must_use]
    pub fn max_snapshots(&self) -> u32 {
        self.max_snapshots
    }

    /// Maximum aggregate snapshot bytes reserved by this daemon.
    #[must_use]
    pub fn max_snapshot_bytes(&self) -> u64 {
        self.max_snapshot_bytes
    }

    /// Whether active qualification proved pause, rootfs export and volatile `/run` exclusion.
    #[must_use]
    pub fn supports_snapshots(&self) -> bool {
        self.contract.snapshots
    }

    /// Recheck current Broker and engine continuity against the admitted contract.
    pub async fn ensure_active(&self) -> Result<(), PlatformError> {
        let (status, value) = self
            .engine
            .json(Method::GET, "/open-compute/v1/contract", Value::Null)
            .await?;
        if status != 200 {
            return Err(invalid());
        }
        let contract: ProviderContract = serde_json::from_value(value).map_err(|_| invalid())?;
        if !contract.qualified || contract.sha256()? != self.contract_sha256 {
            return Err(invalid());
        }
        Ok(())
    }

    /// Immutable contract identity admitted into every application target.
    #[must_use]
    pub fn contract_sha256(&self) -> &str {
        &self.contract_sha256
    }

    /// Resource ceiling verified by Broker qualification.
    #[must_use]
    pub fn resources(&self) -> ContainerResources {
        self.resources
    }

    /// Instance reservation limit selected by the operator.
    #[must_use]
    pub fn max_instances(&self) -> u32 {
        self.max_instances
    }

    /// Qualified maximum bytes in a Docker-save materialization spool.
    #[must_use]
    pub fn max_image_bytes(&self) -> u64 {
        self.contract.max_image_bytes
    }

    /// Immutable interceptor configuration ID supplied only to trusted runtime preparation.
    #[must_use]
    pub fn sidecar_image(&self) -> &str {
        &self.sidecar_image
    }

    /// Qualified OCI architecture used for image admission.
    #[must_use]
    pub fn architecture(&self) -> &str {
        &self.contract.architecture
    }

    /// Obtain generation-scoped credentials through the private authority channel.
    pub async fn lease_credentials(
        &self,
        key: &str,
        generation_sha256: &str,
    ) -> Result<super::BrokerControlCredentials, PlatformError> {
        if !super::contract::opaque(key) || !super::contract::opaque(generation_sha256) {
            return Err(invalid());
        }
        let (status, value) = self
            .engine
            .json(
                Method::POST,
                &format!("/open-compute/v1/leases/{key}/credentials"),
                serde_json::json!({"generationSha256":generation_sha256}),
            )
            .await?;
        if status != 200 {
            return Err(invalid());
        }
        let credentials: super::BrokerControlCredentials =
            serde_json::from_value(value).map_err(|_| invalid())?;
        credentials.validate()?;
        Ok(credentials)
    }

    /// Register a persisted lease before workerd can create its application or sidecar.
    pub async fn register(&self, lease: &BrokerLease) -> Result<(), PlatformError> {
        self.register_lease(lease, "/open-compute/v1/leases").await
    }

    /// Adopt immutable authority only to clean an execution, retaining every lifecycle fence.
    pub(crate) async fn adopt_for_cleanup(&self, lease: &BrokerLease) -> Result<(), PlatformError> {
        self.register_lease(lease, "/open-compute/v1/leases/cleanup")
            .await
    }

    async fn register_lease(&self, lease: &BrokerLease, path: &str) -> Result<(), PlatformError> {
        lease.validate(&self.contract)?;
        lease.resources.validate(self.resources)?;
        let (status, _) = self
            .engine
            .json(
                Method::POST,
                path,
                serde_json::to_value(lease).map_err(|_| invalid())?,
            )
            .await?;
        if status != 201 && status != 200 {
            return Err(invalid());
        }
        Ok(())
    }

    /// Fence future mutations and confirm removal of both members of one execution identity.
    pub async fn terminate(&self, lease_key: &str) -> Result<(), PlatformError> {
        if !super::contract::opaque(lease_key) {
            return Err(invalid());
        }
        let (status, _) = self
            .engine
            .json(
                Method::DELETE,
                &format!("/open-compute/v1/leases/{lease_key}"),
                Value::Null,
            )
            .await?;
        if status != 204 {
            return Err(invalid());
        }
        Ok(())
    }

    /// Fence new commands, allow a bounded SIGTERM grace, then confirm cleanup.
    pub async fn terminate_gracefully(
        &self,
        lease_key: &str,
        grace_ms: u64,
    ) -> Result<(), PlatformError> {
        if !super::contract::opaque(lease_key) || grace_ms > 900_000 {
            return Err(invalid());
        }
        let (status, _) = self
            .engine
            .json(
                Method::POST,
                &format!("/open-compute/v1/leases/{lease_key}/stop"),
                serde_json::json!({"graceMillis":grace_ms}),
            )
            .await?;
        if status != 204 {
            return Err(invalid());
        }
        Ok(())
    }

    /// Observe a registered lease using provider-verified identity and process state.
    pub async fn observe(
        &self,
        lease_key: &str,
    ) -> Result<ProviderLeaseObservation, PlatformError> {
        if !super::contract::opaque(lease_key) {
            return Err(invalid());
        }
        let (status, value) = self
            .engine
            .json(
                Method::GET,
                &format!("/open-compute/v1/leases/{lease_key}"),
                Value::Null,
            )
            .await?;
        if status != 200 {
            return Err(invalid());
        }
        serde_json::from_value(value).map_err(|_| invalid())
    }

    /// Return the actual engine image size after verifying the admitted configuration and ordered rootfs.
    /// Only an explicit engine absence permits a later offline import.
    pub async fn image_size(
        &self,
        image: &str,
        diff_ids: &[String],
    ) -> Result<Option<u64>, PlatformError> {
        let proof = super::contract::ImageProof {
            image: image.to_owned(),
            diff_ids: diff_ids.to_vec(),
        };
        proof.validate()?;
        let (status, value) = self
            .engine
            .json(
                Method::POST,
                "/open-compute/v1/images/check",
                serde_json::to_value(proof).map_err(|_| invalid())?,
            )
            .await?;
        match status {
            200 => {
                let size: super::contract::ImageSize =
                    serde_json::from_value(value).map_err(|_| invalid())?;
                Ok(Some(size.size))
            }
            404 => Ok(None),
            _ => Err(invalid()),
        }
    }

    /// Import an already verified offline Docker archive through the private materialization route.
    pub async fn materialize(
        &self,
        archive: std::fs::File,
        size: u64,
    ) -> Result<(), PlatformError> {
        if size == 0 || size > self.contract.max_image_bytes {
            return Err(invalid());
        }
        let response = self
            .engine
            .send_archive(
                Method::POST,
                "/open-compute/v1/images/load",
                tokio::fs::File::from_std(archive),
                0..size,
            )
            .await?;
        if response.status().as_u16() != 200 {
            return Err(invalid());
        }
        let _ = bounded_response(response, 1 << 20).await?;
        Ok(())
    }
}
