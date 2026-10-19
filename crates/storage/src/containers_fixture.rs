use super::*;

pub(super) fn fixture_with_policy(max_instances: u32, durable_object: bool) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let storage =
        PlatformStorage::bootstrap(&configuration(&directory.path().join("data")), &SystemClock)
            .unwrap();
    let instance = storage.identity().instance_id;
    let (worker, _) = WorkerRepository::new(storage.db())
        .create_worker(instance, "container", RequestId::generate(), 10, 1_000_000)
        .unwrap();
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
                request_fingerprint: &[3; 32],
                resource_id: namespace,
                driver_schema_version: DO_NAMESPACE_SCHEMA_VERSION,
                request_id: RequestId::generate(),
                now_ms: 11,
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
        .ensure_namespace(&resource, worker.id, "ContainerClass")
        .unwrap();
    resources.mark_ready(namespace, 12).unwrap();
    let repo = ContainerAuthority::new(storage.db());
    let application = if durable_object {
        namespace
    } else {
        ResourceId::generate()
    };
    repo.admit_image(instance, &image(1)).unwrap();
    let id = version(&storage, worker.id);
    repo.declare_version(
        instance,
        id,
        namespace,
        "container-app",
        &declaration_digest(namespace, "container-app"),
    )
    .unwrap();
    let route = activate(&storage, worker.id, id);
    let object = insert_object(&storage, namespace, 7);
    let spec = ContainerTargetSpec {
        worker_version_id: id,
        image_sha256: Some([1; 32]),
        provider_contract_sha256: [8; 32],
        resources: ContainerResources {
            cpu_nanos: if durable_object {
                1_000_000_000
            } else {
                62_500_000
            },
            memory_bytes: if durable_object {
                1 << 30
            } else {
                256 * 1024 * 1024
            },
            disk_bytes: if durable_object {
                4_000_000_000
            } else {
                1024 * 1024 * 1024
            },
            pids: 64,
            open_files: 1024,
        },
        enable_internet: false,
    };
    if durable_object {
        repo.create_durable_object_application(
            instance,
            &ContainerApplication {
                id: application,
                namespace_resource_id: namespace,
                name: "container-app".into(),
                scheduling_policy: ContainerSchedulingPolicy::DurableObject,
                max_instances,
                target_generation: None,
                admission_paused: true,
                created_at_ms: 12,
            },
            &ContainerTargetSpec {
                image_sha256: None,
                ..spec.clone()
            },
            false,
            20,
        )
        .unwrap();
    } else {
        repo.create_application(
            instance,
            application,
            namespace,
            "container-app",
            max_instances,
            12,
        )
        .unwrap();
        publish_target(repo, instance, application, &spec, b"{}", 20).unwrap();
    }
    repo.set_admission_paused(instance, application, false)
        .unwrap();
    Fixture {
        directory,
        storage,
        worker: worker.id,
        spec,
        authority: ContainerObjectAuthority {
            instance_id: instance,
            application_id: application,
            namespace_resource_id: namespace,
            object_id: object,
            object_generation: 1,
            route_generation: route,
            descriptor_sha256: declaration_digest(namespace, "container-app"),
        },
    }
}
