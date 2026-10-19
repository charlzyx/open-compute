use super::*;
use open_compute_core::{InstanceId, VersionId, WorkerId};
use open_compute_storage::containers::{ContainerAuthority, VersionContainerDeclaration};

pub(super) fn resolve(
    storage: &PlatformStorage,
    instance: InstanceId,
    worker: WorkerId,
    version: VersionId,
) -> Result<(Vec<VersionContainerDeclaration>, Vec<RuntimeContainerClass>), PlatformError> {
    let declarations =
        ContainerAuthority::new(storage.db()).version_declarations(instance, version)?;
    let namespaces = DurableObjectRepository::new(storage);
    let classes = declarations
        .iter()
        .map(|declaration| {
            declaration.validate()?;
            let namespace =
                namespaces.get_namespace(instance, declaration.namespace_resource_id)?;
            if namespace.owner_worker_id != worker {
                return Err(invariant());
            }
            Ok(RuntimeContainerClass {
                class_name: namespace.class_name,
                namespace_resource_id: declaration.namespace_resource_id.to_string(),
                application_name: declaration.application_name.clone(),
                descriptor_sha256: hex::encode(declaration.descriptor_sha256),
                images: declaration.images.clone(),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((declarations, classes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use open_compute_core::config::DataConfig;
    use open_compute_core::{RequestId, ResourceId, SystemClock};
    use open_compute_storage::durable_objects::DO_NAMESPACE_SCHEMA_VERSION;
    use open_compute_storage::resources::{
        ReserveResourceCreate, ResourceCreateReservation, ResourceRepository,
    };
    use open_compute_storage::worker_repository::{NewVersion, NewVersionProducts};

    #[test]
    fn container_snapshot_projects_declared_class_without_an_application_or_lease() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("data");
        let storage = PlatformStorage::bootstrap(
            &DataConfig {
                path: root.clone(),
                master_key_file: root.join("keys/master.key"),
                master_key_env: None,
                sqlite_busy_timeout_ms: 5000,
                free_space_soft_bytes: 1_073_741_824,
                free_space_hard_bytes: 1,
            },
            &SystemClock,
        )
        .unwrap();
        let instance = storage.identity().instance_id;
        let workers = WorkerRepository::new(storage.db());
        let worker = workers
            .create_worker(
                instance,
                "container-source",
                RequestId::generate(),
                1,
                1_000_000,
            )
            .unwrap()
            .0
            .id;
        let namespace = ResourceId::generate();
        let resources = ResourceRepository::new(storage.db());
        let resource = match resources
            .reserve_create(
                &ReserveResourceCreate {
                    instance_id: instance,
                    kind: BindingKind::DoNamespace,
                    name: "CONTAINER",
                    idempotency_key: "container",
                    fingerprint_key_id: "key",
                    request_fingerprint: &[1; 32],
                    resource_id: namespace,
                    driver_schema_version: DO_NAMESPACE_SCHEMA_VERSION,
                    request_id: RequestId::generate(),
                    now_ms: 1,
                    expires_at_ms: 1000,
                },
                1_000_000,
            )
            .unwrap()
        {
            ResourceCreateReservation::Reserved(resource) => resource,
            other => panic!("unexpected {other:?}"),
        };
        DurableObjectRepository::new(&storage)
            .ensure_namespace(&resource, worker, "ApiContainer")
            .unwrap();
        resources.mark_ready(namespace, 2).unwrap();
        let version = VersionId::generate();
        let mut input = NewVersion {
            id: version,
            instance_id: instance,
            worker_id: worker,
            content_kind: VersionContentKind::Worker,
            artifact_sha256: Some([1; 32]),
            artifact_size: Some(1),
            artifact_schema_version: Some(1),
            main_module: Some("index.js".into()),
            worker_code_sha256: [2; 32],
            compatibility_date: "2026-09-08".into(),
            compatibility_flags: vec![],
            resource_limits: EffectiveResourceLimits::standard_defaults(),
            vars: BTreeMap::new(),
            secrets: BTreeMap::new(),
            request_id: RequestId::generate(),
            now_ms: 3,
        };
        workers
            .insert_staging_version(&input, &NewVersionProducts::default(), 1_000_000)
            .unwrap();
        assert_eq!(
            resolve(&storage, instance, worker, version).unwrap(),
            (vec![], vec![])
        );
        ContainerAuthority::new(storage.db())
            .admit_image(
                instance,
                &open_compute_storage::containers::ContainerImage {
                    manifest_sha256: [3; 32],
                    config_sha256: [4; 32],
                    diff_ids: vec![[5; 32]],
                    architecture: "arm64".into(),
                    compressed_bytes: 1,
                    unpacked_bytes: 1,
                    artifact_sha256: [6; 32],
                    artifact_size: 1,
                    created_at_ms: 1,
                },
            )
            .unwrap();
        let declaration = VersionContainerDeclaration::with_images(
            namespace,
            "api-container".into(),
            BTreeMap::from([(
                "main".into(),
                format!("registry/app@sha256:{}", hex::encode([3; 32])),
            )]),
        )
        .unwrap();
        let version = VersionId::generate();
        input.id = version;
        workers
            .insert_staging_version(
                &input,
                &NewVersionProducts {
                    containers: std::slice::from_ref(&declaration),
                    ..NewVersionProducts::default()
                },
                1_000_000,
            )
            .unwrap();
        let (declared, classes) = resolve(&storage, instance, worker, version).unwrap();
        assert_eq!(declared, vec![declaration.clone()]);
        assert_eq!(
            classes,
            vec![RuntimeContainerClass {
                class_name: "ApiContainer".into(),
                namespace_resource_id: namespace.to_string(),
                application_name: declaration.application_name,
                images: declaration.images,
                descriptor_sha256: hex::encode(declaration.descriptor_sha256)
            }]
        );
        assert_eq!(
            serde_json::to_value(&classes).unwrap()[0]["className"],
            "ApiContainer"
        );
        assert!(resolve(&storage, instance, WorkerId::generate(), version).is_err());
        assert!(resolve(&storage, InstanceId::generate(), worker, version).is_err());
    }
}
