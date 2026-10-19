use super::*;
use open_compute_artifacts::{ArtifactRef, ContainerImageLimits};
use open_compute_storage::containers::{
    ContainerApplication, ContainerApplicationMetadataPatch, ContainerImage, ContainerTargetSpec,
};
use std::os::unix::fs::PermissionsExt;

impl ContainerService {
    pub(crate) async fn create_application(
        &self,
        input: &wire::ApplicationCreate,
        namespace: ResourceId,
        now_ms: i64,
    ) -> Result<ContainerApplication, PlatformError> {
        let mut input = input.clone();
        if let Some(configuration) = input.configuration.take() {
            input.configuration = Some(self.normalize_configuration(configuration)?);
        }
        let resources = input.validate(self.provider.resources(), self.provider.max_instances())?;
        let repo = ContainerAuthority::new(self.storage.db());
        let version =
            repo.active_declaration_version(self.instance_id(), namespace, &input.name)?;
        self.ensure_active().await?;
        if input.scheduling_policy
            == open_compute_storage::containers::ContainerSchedulingPolicy::DurableObject
        {
            let application = ContainerApplication {
                id: namespace,
                namespace_resource_id: namespace,
                name: input.name.clone(),
                scheduling_policy: input.scheduling_policy,
                max_instances: self.provider.max_instances(),
                target_generation: None,
                admission_paused: true,
                created_at_ms: now_ms,
            };
            repo.create_durable_object_application(
                self.instance_id(),
                &application,
                &ContainerTargetSpec {
                    worker_version_id: version,
                    image_sha256: None,
                    provider_contract_sha256: digest(self.provider.contract_sha256())?,
                    resources,
                    enable_internet: true,
                },
                input
                    .observability
                    .as_ref()
                    .is_some_and(|value| value.logs.enabled),
                now_ms,
            )?;
            return repo.application_for_namespace(self.instance_id(), namespace, &input.name);
        }
        let input_configuration = input.configuration.as_ref().ok_or_else(wire::invalid)?;
        let image_digest = input_configuration.image_digest()?;
        self.ensure_active().await?;
        repo.ordinary_image(self.instance_id(), &image_digest)?;
        self.admitted_image(&image_digest)?;
        let id = ResourceId::generate();
        let configuration = serde_json::to_vec(input_configuration).map_err(|_| unavailable())?;
        let application = ContainerApplication {
            id,
            namespace_resource_id: namespace,
            name: input.name.clone(),
            scheduling_policy: open_compute_storage::containers::ContainerSchedulingPolicy::Default,
            max_instances: input.max_instances.ok_or_else(wire::invalid)?,
            target_generation: None,
            admission_paused: true,
            created_at_ms: now_ms,
        };
        repo.commit_application_configuration(
            self.instance_id(),
            &application,
            &ContainerTargetSpec {
                worker_version_id: version,
                image_sha256: Some(image_digest),
                provider_contract_sha256: digest(self.provider.contract_sha256())?,
                resources,
                enable_internet: true,
            },
            &configuration,
            ContainerApplicationMetadataPatch {
                max_instances: input.max_instances,
                observability_logs: Some(
                    input
                        .observability
                        .as_ref()
                        .is_some_and(|value| value.logs.enabled),
                ),
            },
            now_ms,
        )?;
        repo.application_for_namespace(self.instance_id(), namespace, &input.name)
    }

    pub(crate) async fn update_application(
        &self,
        id: ResourceId,
        input: &wire::ApplicationPatch,
        now_ms: i64,
    ) -> Result<ContainerApplication, PlatformError> {
        self.ensure_active().await?;
        let mut input = input.clone();
        if let Some(configuration) = input.configuration.take() {
            input.configuration = Some(self.normalize_configuration(configuration)?);
        }
        let resources = input.validate(self.provider.resources(), self.provider.max_instances())?;
        let repo = ContainerAuthority::new(self.storage.db());
        let app = repo
            .applications(self.instance_id())?
            .into_iter()
            .find(|app| app.id == id)
            .ok_or_else(wire::invalid)?;
        if app.scheduling_policy
            == open_compute_storage::containers::ContainerSchedulingPolicy::DurableObject
        {
            if input.configuration.is_some()
                || input.max_instances.is_some()
                || input.constraints.is_some()
                || input.scheduling_policy.is_some()
                || input.rollout_active_grace_period.is_some()
            {
                return Err(wire::invalid());
            }
            repo.patch_application_metadata(
                self.instance_id(),
                id,
                ContainerApplicationMetadataPatch {
                    max_instances: None,
                    observability_logs: input
                        .observability
                        .as_ref()
                        .map(|value| value.logs.enabled),
                },
                now_ms,
            )?;
            return repo.application_for_namespace(
                self.instance_id(),
                app.namespace_resource_id,
                &app.name,
            );
        }
        self.application_configuration(&app)?;
        let version = repo.active_declaration_version(
            self.instance_id(),
            app.namespace_resource_id,
            &app.name,
        )?;
        let metadata = ContainerApplicationMetadataPatch {
            max_instances: input.max_instances,
            observability_logs: input.observability.as_ref().map(|value| value.logs.enabled),
        };
        if let Some(configuration) = input.configuration {
            let image_digest = configuration.image_digest()?;
            repo.ordinary_image(self.instance_id(), &image_digest)?;
            self.admitted_image(&image_digest)?;
            let configuration = serde_json::to_vec(&configuration).map_err(|_| unavailable())?;
            repo.commit_application_configuration(
                self.instance_id(),
                &app,
                &ContainerTargetSpec {
                    worker_version_id: version,
                    image_sha256: Some(image_digest),
                    provider_contract_sha256: digest(self.provider.contract_sha256())?,
                    resources: resources.ok_or_else(unavailable)?,
                    enable_internet: true,
                },
                &configuration,
                metadata,
                now_ms,
            )?;
        } else {
            repo.patch_application_metadata(self.instance_id(), id, metadata, now_ms)?;
        }
        repo.application_for_namespace(self.instance_id(), app.namespace_resource_id, &app.name)
    }

    pub(crate) fn application_configuration(
        &self,
        app: &ContainerApplication,
    ) -> Result<(wire::Configuration, bool, i64), PlatformError> {
        let repo = ContainerAuthority::new(self.storage.db());
        let (bytes, logs, updated_at_ms) = repo.application_settings(self.instance_id(), app.id)?;
        let configuration: wire::Configuration =
            serde_json::from_slice(bytes.as_deref().ok_or_else(unavailable)?)
                .map_err(|_| unavailable())?;
        let resources = configuration.resources(self.provider.resources())?;
        let target = repo.target(
            self.instance_id(),
            app.id,
            app.target_generation.ok_or_else(unavailable)?,
        )?;
        if target.spec.image_sha256 != Some(configuration.image_digest()?)
            || target.spec.resources != resources
        {
            return Err(unavailable());
        }
        Ok((configuration, logs, updated_at_ms))
    }

    pub(crate) async fn prepare_image(&self, reference: &str) -> Result<(), PlatformError> {
        let image_digest = open_compute_storage::containers::registry_image_digest(reference)
            .map_err(|_| wire::invalid())?;
        self.ensure_active().await?;
        ContainerAuthority::new(self.storage.db())
            .ordinary_image(self.instance_id(), &image_digest)?;
        let image = self.admitted_image(&image_digest)?;
        let artifact = ArtifactRef::new(
            open_compute_artifacts::ARTIFACT_KEY_VERSION,
            &hex::encode(image.artifact_sha256),
            image.artifact_size,
        )?;
        let _pin = self.cache.acquire(&self.artifacts, &artifact).await?;
        Ok(())
    }

    pub(super) fn admitted_image(
        &self,
        image_digest: &[u8; 32],
    ) -> Result<ContainerImage, PlatformError> {
        let image =
            ContainerAuthority::new(self.storage.db()).image(self.instance_id(), image_digest)?;
        let transport_budget = image
            .unpacked_bytes
            .checked_add(4 << 20)
            .ok_or_else(unavailable)?;
        if image.architecture != self.provider.architecture()
            || image.artifact_size > self.provider.max_image_bytes()
            || image.compressed_bytes > self.provider.max_image_bytes()
            || transport_budget > self.provider.max_image_bytes()
        {
            return Err(wire::invalid());
        }
        ArtifactRef::new(
            open_compute_artifacts::ARTIFACT_KEY_VERSION,
            &hex::encode(image.artifact_sha256),
            image.artifact_size,
        )?;
        Ok(image)
    }

    pub(super) async fn materialize_image(
        &self,
        image_digest: &[u8; 32],
    ) -> Result<u64, PlatformError> {
        let image = self.admitted_image(image_digest)?;
        let image_ref = format!("sha256:{}", hex::encode(image.config_sha256));
        let diff_ids = image
            .diff_ids
            .iter()
            .map(|digest| format!("sha256:{}", hex::encode(digest)))
            .collect::<Vec<_>>();
        if let Some(size) = self
            .provider
            .image_size(&image_ref, &diff_ids)
            .await
            .map_err(|_| unavailable())?
        {
            return Ok(size);
        }
        let artifact = ArtifactRef::new(
            open_compute_artifacts::ARTIFACT_KEY_VERSION,
            &hex::encode(image.artifact_sha256),
            image.artifact_size,
        )?;
        let spool_limit = image
            .unpacked_bytes
            .checked_add(4 << 20)
            .ok_or_else(unavailable)?;
        if spool_limit > self.provider.max_image_bytes() {
            return Err(wire::invalid());
        }
        let disk_bytes = image
            .artifact_size
            .checked_add(spool_limit)
            .ok_or_else(unavailable)?;
        let _reservation = self.storage.reserve_mutation(disk_bytes)?;
        let pin = self.cache.acquire(&self.artifacts, &artifact).await?;
        let temporary = self.storage.data_dir().prepare_tmp_dir()?;
        let (file, size) = tokio::task::spawn_blocking(move || {
            let limits = ContainerImageLimits {
                json_bytes: 1 << 20,
                compressed_bytes: image.compressed_bytes,
                unpacked_bytes: image.unpacked_bytes,
                layers: 128,
                entries: 100_000,
            };
            let output = tempfile::tempfile_in(temporary).map_err(|_| unavailable())?;
            output
                .set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|_| unavailable())?;
            let output = open_compute_artifacts::write_container_docker_archive(
                pin.file(),
                &artifact,
                image.manifest_sha256,
                &image.architecture,
                limits,
                std::io::BufWriter::with_capacity(256 * 1024, output),
            )?;
            let mut output = output.into_inner().map_err(|_| unavailable())?;
            use std::io::Seek as _;
            let size = output.metadata().map_err(|_| unavailable())?.len();
            if size > spool_limit {
                return Err(unavailable());
            }
            output.rewind().map_err(|_| unavailable())?;
            Ok::<_, PlatformError>((output, size))
        })
        .await
        .map_err(|_| unavailable())??;
        self.provider
            .materialize(file, size)
            .await
            .map_err(|_| unavailable())?;
        self.provider
            .image_size(&image_ref, &diff_ids)
            .await
            .map_err(|_| unavailable())?
            .ok_or_else(unavailable)
    }

    fn normalize_configuration(
        &self,
        mut configuration: wire::Configuration,
    ) -> Result<wire::Configuration, PlatformError> {
        if configuration.image_digest().is_ok() {
            return Ok(configuration);
        }
        let (host, path) = configuration
            .image
            .split_once('/')
            .ok_or_else(wire::invalid)?;
        if host.is_empty()
            || host
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        {
            return Err(wire::invalid());
        }
        let (repository, tag) = path.rsplit_once(':').ok_or_else(wire::invalid)?;
        let digest = ContainerAuthority::new(self.storage.db()).registry_tag(
            self.instance_id(),
            repository,
            tag,
        )?;
        configuration.image = format!("{host}/{repository}@sha256:{}", hex::encode(digest));
        configuration.image_digest()?;
        Ok(configuration)
    }
}
