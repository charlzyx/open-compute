use super::*;

impl ContainerService {
    /// Resume persisted rollout cohorts without replacing uncertain executions or their targets.
    pub(crate) async fn reconcile_rollouts(
        &self,
        transport: &crate::runtime_bridge::WorkerdTransport,
    ) -> Result<(), PlatformError> {
        let repo = ContainerAuthority::new(self.storage.db());
        for id in
            repo.expire_registry_staging(self.instance_id(), open_compute_core::wall_time_ms())?
        {
            let path = self.registry_upload_path(id)?;
            match tokio::fs::remove_file(path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(unavailable()),
            }
        }
        let owned = repo.registry_upload_ids(self.instance_id())?;
        let mut entries = tokio::fs::read_dir(self.storage.data_dir().prepare_tmp_dir()?)
            .await
            .map_err(|_| unavailable())?;
        while let Some(entry) = entries.next_entry().await.map_err(|_| unavailable())? {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(id) = name
                .strip_prefix("container-registry-")
                .and_then(|n| n.strip_suffix(".part"))
                .and_then(|s| s.parse::<ResourceId>().ok())
            else {
                continue;
            };
            if !owned.contains(&id) {
                tokio::fs::remove_file(self.registry_upload_path(id)?)
                    .await
                    .map_err(|_| unavailable())?;
            }
        }
        self.reconcile_snapshots().await?;
        for application in repo.pending_application_deletions(self.instance_id())? {
            self.finish_application_deletion(application).await?;
        }
        for app in repo.applications(self.instance_id())? {
            for lease in repo.application_leases(self.instance_id(), app.id)? {
                if lease.state != ContainerLeaseState::Stopped
                    && lease.stop_requested_at_ms.is_some()
                {
                    self.finish_maintenance_stop(&lease, Some(0)).await?;
                }
            }
            for rollout in repo.rollouts(self.instance_id(), app.id)? {
                if rollout.state != "running" {
                    continue;
                }
                let leases = repo.rollout_step_leases(
                    self.instance_id(),
                    app.id,
                    rollout.id,
                    open_compute_core::wall_time_ms(),
                )?;
                for lease in leases {
                    self.finish_maintenance_stop(&lease, Some(0)).await?;
                }
                for lease in repo.rollout_replacements(self.instance_id(), app.id, rollout.id)? {
                    transport
                        .start_container(
                            lease.identity.namespace_resource_id,
                            lease.identity.object_id,
                            lease.identity.object_generation,
                        )
                        .await?;
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn delete_application(
        &self,
        application: ResourceId,
    ) -> Result<(), PlatformError> {
        ContainerAuthority::new(self.storage.db()).begin_application_delete(
            self.instance_id(),
            application,
            open_compute_core::wall_time_ms(),
        )?;
        self.finish_application_deletion(application).await
    }

    async fn finish_application_deletion(
        &self,
        application: ResourceId,
    ) -> Result<(), PlatformError> {
        let repo = ContainerAuthority::new(self.storage.db());
        for lease in repo.application_leases(self.instance_id(), application)? {
            if lease.state == ContainerLeaseState::Stopped {
                continue;
            }
            self.finish_maintenance_stop(&lease, None).await?;
        }
        self.reconcile_snapshots().await?;
        repo.finish_application_delete(
            self.instance_id(),
            application,
            open_compute_core::wall_time_ms(),
        )
    }

    pub(super) async fn finish_maintenance_stop(
        &self,
        lease: &open_compute_storage::containers::ContainerLease,
        grace_millis: Option<u64>,
    ) -> Result<(), PlatformError> {
        let repo = ContainerAuthority::new(self.storage.db());
        if self.terminate_lease(lease, grace_millis).await.is_err() {
            repo.mark_lost(
                lease.identity,
                &lease.opaque_engine_key,
                open_compute_core::wall_time_ms(),
            )?;
            return Err(unavailable());
        }
        repo.finish_stopped(
            lease.identity,
            &ContainerTermination {
                opaque_engine_key: &lease.opaque_engine_key,
                start_identity: lease.start_identity.as_deref(),
                terminated: true,
                cleanup_complete: true,
                late_operations_fenced: true,
            },
            open_compute_core::wall_time_ms(),
        )
    }
}
