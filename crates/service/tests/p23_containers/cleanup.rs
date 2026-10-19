use open_compute_core::{InstanceId, config::ContainersConfig, containers::ContainerLeaseState};
use open_compute_service::container_broker::QualifiedContainerProvider;
use open_compute_storage::{containers::ContainerAuthority, control_db::ControlDb};
use std::path::PathBuf;

/// Reap only this fresh fixture instance's owned leases even if an assertion unwinds.
pub(super) struct Cleanup {
    pub data: PathBuf,
    pub instance: InstanceId,
    pub provider: ContainersConfig,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let data = self.data.clone();
        let instance = self.instance;
        let config = self.provider.clone();
        let result = std::thread::spawn(move || -> Result<(), &'static str> {
            let db = ControlDb::open_readonly_wal_aware(&data.join("control.sqlite"), 1000)
                .map_err(|_| "Container fixture cleanup cannot read its authority")?;
            let repo = ContainerAuthority::new(&db);
            let applications = repo
                .applications(instance)
                .map_err(|_| "Container fixture cleanup cannot read its applications")?;
            let leases = applications
                .into_iter()
                .map(|app| repo.application_leases(instance, app.id))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| "Container fixture cleanup cannot read its leases")?
                .into_iter()
                .flatten()
                .filter(|lease| lease.state != ContainerLeaseState::Stopped)
                .collect::<Vec<_>>();
            if leases.is_empty() {
                return Ok(());
            }
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| "Container fixture cleanup runtime is unavailable")?;
            runtime.block_on(async {
                let provider = QualifiedContainerProvider::preflight(&config)
                    .await
                    .map_err(|_| "Container fixture cleanup provider is unavailable")?;
                for lease in leases {
                    provider
                        .terminate(&lease.opaque_engine_key)
                        .await
                        .map_err(|_| "Container fixture cleanup remains uncertain")?;
                    let proof = provider
                        .observe(&lease.opaque_engine_key)
                        .await
                        .map_err(|_| "Container fixture cleanup cannot observe terminal proof")?;
                    if !proof.terminated || !proof.cleanup_complete || !proof.late_operations_fenced
                    {
                        return Err("Container fixture cleanup lacks positive terminal proof");
                    }
                }
                Ok(())
            })
        })
        .join();
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) if std::thread::panicking() => eprintln!("{error}"),
            Ok(Err(error)) => panic!("{error}"),
            Err(_) if std::thread::panicking() => {
                eprintln!("Container fixture cleanup thread failed");
            }
            Err(_) => panic!("Container fixture cleanup thread failed"),
        }
    }
}
