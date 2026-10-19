//! Normalize native startup overrides before durable capacity reservation.

use super::*;
use open_compute_storage::containers::{
    ContainerApplication, ContainerExecutionSpec, ContainerLeaseStartup,
};

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Startup {
    pub image: Option<String>,
    pub container_snapshot_id: Option<ResourceId>,
    pub instance_profile: Option<open_compute_core::containers::ContainerInstanceProfile>,
    pub resources: Option<Resources>,
    pub enable_internet: Option<bool>,
    pub entrypoint: Option<Vec<String>>,
    pub environment_variables: Option<Vec<String>>,
    pub labels: Option<Vec<crate::container_broker::BrokerLabel>>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Resources {
    pub vcpu: f64,
    pub memory_mib: u64,
    pub disk_mb: f64,
}

impl ContainerService {
    pub(super) fn startup_execution(
        &self,
        request: &ResolveRequest,
        app: &ContainerApplication,
        authority: &ContainerObjectAuthority,
        engine_key: &str,
        now_ms: i64,
    ) -> Result<ContainerExecutionSpec, PlatformError> {
        let repo = ContainerAuthority::new(self.storage.db());
        let existing = repo.existing_lease(authority)?;
        let replay = if existing.is_none() && request.startup.is_none() {
            repo.replacement_startup(authority)?
        } else {
            None
        };
        let frozen = existing.as_ref().or(replay.as_ref());
        let target = repo.target(
            self.instance_id(),
            app.id,
            existing
                .as_ref()
                .map(|lease| lease.target_generation)
                .or(app.target_generation)
                .ok_or_else(unavailable)?,
        )?;
        let empty = Startup {
            image: None,
            container_snapshot_id: None,
            instance_profile: None,
            resources: None,
            enable_internet: None,
            entrypoint: None,
            environment_variables: None,
            labels: None,
        };
        let startup = request.startup.as_ref().unwrap_or(&empty);
        if app.scheduling_policy
            == open_compute_storage::containers::ContainerSchedulingPolicy::Default
            && (startup.image.is_some()
                || startup.instance_profile.is_some()
                || startup.resources.is_some())
        {
            return Err(wire::invalid());
        }
        if startup.instance_profile
            == Some(open_compute_core::containers::ContainerInstanceProfile::Basic)
        {
            return Err(wire::invalid());
        }
        if (startup.resources.is_some() && startup.instance_profile.is_some())
            || (startup.image.is_some() && startup.container_snapshot_id.is_some())
        {
            return Err(unavailable());
        }
        let snapshot_id = if request.startup.is_some() {
            startup.container_snapshot_id
        } else if existing.is_some() {
            frozen.and_then(|lease| lease.startup.snapshot_id)
        } else {
            // A rollout starts its new target image; a snapshot is an explicit restore choice.
            None
        };
        if snapshot_id.is_some()
            && app.scheduling_policy
                != open_compute_storage::containers::ContainerSchedulingPolicy::DurableObject
        {
            return Err(wire::invalid());
        }
        let restored = if let Some(id) = snapshot_id.filter(|_| existing.is_none()) {
            Some(repo.snapshot_for_restore(authority, id, now_ms)?)
        } else {
            None
        };
        let image_sha256 = if let Some(image) = &restored {
            image.manifest_sha256
        } else {
            match &startup.image {
                Some(image) => wire::Configuration {
                    image: image.clone(),
                    instance_type: None,
                    vcpu: None,
                    memory_mib: None,
                    disk: None,
                }
                .image_digest()?,
                None => frozen
                    .filter(|lease| existing.is_some() || lease.startup.image_override)
                    .map(|lease| lease.effective_image_sha256)
                    .or(target.spec.image_sha256)
                    .ok_or_else(wire::invalid)?,
            }
        };
        let image = if snapshot_id.is_some() {
            repo.image(self.instance_id(), &image_sha256)?
        } else {
            repo.ordinary_image(self.instance_id(), &image_sha256)?
        };
        if image.architecture != self.provider.architecture() {
            return Err(unavailable());
        }
        let resources = if let Some(resources) = &startup.resources {
            wire::Configuration {
                image: format!("startup@sha256:{}", hex::encode(image_sha256)),
                instance_type: None,
                vcpu: Some(resources.vcpu),
                memory_mib: Some(resources.memory_mib),
                disk: Some(wire::Disk {
                    size_mb: resources.disk_mb,
                }),
            }
            .resources(target.spec.resources)?
        } else if let Some(name) = &startup.instance_profile {
            wire::Configuration {
                image: format!("startup@sha256:{}", hex::encode(image_sha256)),
                instance_type: Some(name.as_str().to_owned()),
                vcpu: None,
                memory_mib: None,
                disk: None,
            }
            .resources(target.spec.resources)?
        } else if let Some(lease) =
            frozen.filter(|lease| existing.is_some() || lease.startup.resources_override)
        {
            lease.effective_resources
        } else if app.scheduling_policy
            == open_compute_storage::containers::ContainerSchedulingPolicy::DurableObject
        {
            open_compute_core::containers::ContainerInstanceProfile::Lite
                .resources(target.spec.resources)?
        } else {
            target.spec.resources
        };
        resources.validate(self.provider.resources())?;
        let options = if request.startup.is_none() {
            frozen
                .map(|lease| self.start_options(lease))
                .transpose()?
                .unwrap_or(crate::container_broker::BrokerStartOptions {
                    enable_internet: target.spec.enable_internet,
                    entrypoint: None,
                    environment_variables: Vec::new(),
                    labels: Vec::new(),
                })
        } else {
            crate::container_broker::BrokerStartOptions {
                enable_internet: startup
                    .enable_internet
                    .unwrap_or(target.spec.enable_internet),
                entrypoint: startup.entrypoint.clone(),
                environment_variables: startup.environment_variables.clone().unwrap_or_default(),
                labels: startup.labels.clone().unwrap_or_default(),
            }
        };
        options.validate()?;
        let payload = serde_json::to_vec(&options).map_err(|_| unavailable())?;
        let owner = ContainerLeaseIdentity {
            namespace_resource_id: authority.namespace_resource_id,
            object_id: authority.object_id,
            object_generation: authority.object_generation,
            lease_generation: 1,
        }
        .owner_key(self.instance_id());
        let envelope = self.storage.crypto().seal_container_startup(
            &open_compute_core::SecretBytes::new(payload),
            self.instance_id(),
            &owner,
            engine_key,
        )?;
        if let Some(lease) = &existing
            && self.start_options(lease)? != options
        {
            return Err(unavailable());
        }
        let mut startup_metadata = if request.startup.is_none() {
            frozen
                .map(|lease| lease.startup.clone())
                .unwrap_or_default()
        } else {
            ContainerLeaseStartup {
                snapshot_id,
                image_reference: String::new(),
                instance_profile: startup.instance_profile,
                image_override: startup.image.is_some(),
                resources_override: startup.resources.is_some()
                    || startup.instance_profile.is_some(),
            }
        };
        startup_metadata.snapshot_id = snapshot_id;
        startup_metadata.image_reference = if snapshot_id.is_some() {
            String::new()
        } else if let Some(image) = &startup.image {
            image.clone()
        } else if let Some(lease) =
            frozen.filter(|lease| existing.is_some() || lease.startup.image_override)
        {
            lease.startup.image_reference.clone()
        } else {
            self.application_configuration(app)?.0.image
        };
        Ok(ContainerExecutionSpec {
            image_sha256: if request.startup.is_none() {
                existing
                    .as_ref()
                    .map_or(image_sha256, |lease| lease.effective_image_sha256)
            } else {
                image_sha256
            },
            resources: if request.startup.is_none() {
                existing
                    .as_ref()
                    .map_or(resources, |lease| lease.effective_resources)
            } else {
                resources
            },
            enable_internet: options.enable_internet,
            startup: startup_metadata,
            startup_envelope: serde_json::to_vec(&envelope).map_err(|_| unavailable())?,
        })
    }

    pub(super) fn start_options(
        &self,
        lease: &open_compute_storage::containers::ContainerLease,
    ) -> Result<crate::container_broker::BrokerStartOptions, PlatformError> {
        let bytes = &lease.startup_envelope;
        let options = self.open_start_options(
            bytes,
            &lease.identity.owner_key(self.instance_id()),
            &lease.opaque_engine_key,
        )?;
        options.validate()?;
        if options.enable_internet != lease.enable_internet {
            return Err(unavailable());
        }
        Ok(options)
    }
    pub(super) fn open_start_options(
        &self,
        bytes: &[u8],
        owner_key: &str,
        engine_key: &str,
    ) -> Result<crate::container_broker::BrokerStartOptions, PlatformError> {
        let envelope: open_compute_storage::crypto::SecretEnvelope =
            serde_json::from_slice(bytes).map_err(|_| unavailable())?;
        let plaintext = self.storage.crypto().open_container_startup(
            &envelope,
            self.instance_id(),
            owner_key,
            engine_key,
        )?;
        let options: crate::container_broker::BrokerStartOptions =
            serde_json::from_slice(plaintext.expose()).map_err(|_| unavailable())?;
        options.validate()?;
        Ok(options)
    }
}

#[cfg(test)]
mod tests {
    use super::Startup;

    #[test]
    fn container_startup_uses_closed_profiles_and_typed_snapshot_identity() {
        let input: Startup = serde_json::from_value(serde_json::json!({
            "instanceProfile": "lite",
            "enableInternet": false,
            "environmentVariables": ["APP_MODE=test"]
        }))
        .unwrap();
        assert_eq!(input.instance_profile.unwrap().as_str(), "lite");
        assert_eq!(input.enable_internet, Some(false));
        let id = open_compute_core::ResourceId::generate();
        let snapshot: Startup = serde_json::from_value(serde_json::json!({
            "containerSnapshotId": id.to_string()
        }))
        .unwrap();
        assert_eq!(snapshot.container_snapshot_id, Some(id));
        for rejected in [
            serde_json::json!({"instanceProfile":"invoice-1"}),
            serde_json::json!({"instanceName":"lite"}),
            serde_json::json!({"containerSnapshotId":"a".repeat(64)}),
            serde_json::json!({"directorySnapshots":[]}),
        ] {
            assert!(serde_json::from_value::<Startup>(rejected).is_err());
        }
    }
}
