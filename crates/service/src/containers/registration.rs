//! Frozen execution registration shared by start and cleanup-only recovery.

use super::*;
use open_compute_storage::containers::ContainerLease;

impl ContainerService {
    pub(super) fn registration(
        &self,
        lease: &ContainerLease,
    ) -> Result<BrokerLease, PlatformError> {
        let repo = ContainerAuthority::new(self.storage.db());
        let app = repo
            .applications(self.instance_id())?
            .into_iter()
            .find(|app| app.id == lease.application_id)
            .ok_or_else(unavailable)?;
        let target = repo.target(self.instance_id(), app.id, lease.target_generation)?;
        if target.spec.provider_contract_sha256 != digest(self.provider.contract_sha256())? {
            return Err(unavailable());
        }
        lease
            .effective_resources
            .validate(self.provider.resources())?;
        let image = self.admitted_image(&lease.effective_image_sha256)?;
        let directories = repo.directory_lease_inputs(
            self.instance_id(),
            lease.identity,
            &lease.opaque_engine_key,
        )?;
        let directories = directories
            .into_iter()
            .map(|(entry, image)| {
                let archive = image
                    .map(|image| {
                        if self.admitted_image(&image.manifest_sha256)? != image
                            || image.diff_ids.len() != 1
                        {
                            return Err(unavailable());
                        }
                        Ok(crate::container_broker::BrokerDirectoryArchive {
                            sha256: hex::encode(image.diff_ids[0]),
                            bytes: image.unpacked_bytes,
                        })
                    })
                    .transpose()?;
                Ok(crate::container_broker::BrokerDirectoryRestore {
                    mount_point: entry.mount_point,
                    archive,
                })
            })
            .collect::<Result<Vec<_>, PlatformError>>()?;
        Ok(BrokerLease {
            directory_snapshots: directories,
            application_id: app.id,
            scheduling_policy: app.scheduling_policy,
            owner_key: lease.identity.owner_key(self.instance_id()),
            max_instances: app.max_instances,
            lease_key: lease.opaque_engine_key.clone(),
            start_identity: expected_start_identity(&lease.opaque_engine_key),
            manifest_sha256: hex::encode(lease.effective_image_sha256),
            image: format!("sha256:{}", hex::encode(image.config_sha256)),
            diff_ids: image
                .diff_ids
                .iter()
                .map(|digest| format!("sha256:{}", hex::encode(digest)))
                .collect(),
            provider_contract_sha256: self.provider.contract_sha256().into(),
            resources: lease.effective_resources,
            startup: self.start_options(lease)?,
        })
    }

    pub(super) async fn terminate_lease(
        &self,
        lease: &ContainerLease,
        grace_millis: Option<u64>,
    ) -> Result<(), PlatformError> {
        if lease.stop_requested_at_ms.is_none() {
            return Err(unavailable());
        }
        self.provider
            .adopt_for_cleanup(&self.registration(lease)?)
            .await?;
        match grace_millis {
            Some(grace) => {
                self.provider
                    .terminate_gracefully(&lease.opaque_engine_key, grace)
                    .await
            }
            None => self.provider.terminate(&lease.opaque_engine_key).await,
        }
    }
}
