use super::*;
use crate::pipeline::{VersionBindingInput, validate_binding_set};
use serde_json::json;
use sha2::Digest;

#[test]
fn service_props_are_canonical_bounded_and_part_of_identity() {
    let target = WorkerId::generate();
    let descriptor = ServiceDescriptor::new(
        "CATALOG".to_owned(),
        ServiceTarget::Worker { worker_id: target },
        Some("CatalogApi".to_owned()),
        Some(json!({
            "z": [1, {"__proto__": "ordinary JSON data"}],
            "constructor": {"enabled": true},
        })),
    )
    .unwrap();
    let bytes = descriptor.canonical_bytes().unwrap();
    assert!(
        bytes
            .windows(b"constructor".len())
            .any(|part| part == b"constructor")
    );
    assert_ne!(
        descriptor.sha256().unwrap(),
        ServiceDescriptor::new(
            "CATALOG".to_owned(),
            ServiceTarget::Worker { worker_id: target },
            Some("CatalogApi".to_owned()),
            Some(json!({"constructor": {"enabled": false}})),
        )
        .unwrap()
        .sha256()
        .unwrap()
    );

    let oversized = ServiceDescriptor::new(
        "CATALOG".to_owned(),
        ServiceTarget::Worker { worker_id: target },
        None,
        Some(json!({"value": "x".repeat(64 * 1024)})),
    )
    .unwrap_err();
    assert_eq!(oversized.code(), ErrorCode::ResourceLimitExceeded);

    let mut nested = json!(true);
    for _ in 0..33 {
        nested = json!([nested]);
    }
    assert_eq!(
        ServiceDescriptor::new(
            "CATALOG".to_owned(),
            ServiceTarget::Worker { worker_id: target },
            None,
            Some(json!({"nested": nested})),
        )
        .unwrap_err()
        .code(),
        ErrorCode::ResourceLimitExceeded
    );

    let mut corrupted = descriptor;
    corrupted.props = Some(json!([]));
    assert_eq!(
        corrupted.sha256().unwrap_err().code(),
        ErrorCode::BundleInvalid
    );
}

#[test]
fn every_version_binds_the_complete_system_source_identity() {
    let bundle = crate::CanonicalBundle::build(
        "index.js",
        vec![crate::ModuleInput {
            name: "index.js".into(),
            module_type: crate::ModuleType::EsModule,
            bytes: b"export default {}".to_vec(),
        }],
        crate::BundleLimits::default(),
    )
    .unwrap();
    let descriptor = WorkerCodeDescriptorV1::new(
        InstanceId::generate(),
        WorkerId::generate(),
        VersionId::generate(),
        0,
        "2026-09-08".into(),
        Vec::new(),
        open_compute_storage::worker_repository::EffectiveResourceLimits::standard_defaults(),
        Some((bundle.sha256(), bundle.manifest())),
        None,
        BTreeMap::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        CachePolicyDescriptorV1::default(),
        Vec::new(),
        1,
    )
    .unwrap();
    assert_eq!(
        descriptor.system_worker_sources_sha256,
        hex::encode(Sha256::digest(SYSTEM_WORKER_MANIFEST))
    );
    let digest = descriptor.sha256().unwrap();
    let mut changed = descriptor.clone();
    changed.system_worker_sources_sha256 = "00".repeat(32);
    assert_ne!(digest, changed.sha256().unwrap());
    let mut wire = serde_json::to_value(&descriptor).unwrap();
    assert_eq!(
        serde_json::from_value::<WorkerCodeDescriptorV1>(wire.clone()).unwrap(),
        descriptor
    );
    wire.as_object_mut()
        .unwrap()
        .remove("systemWorkerSourcesSha256");
    assert!(serde_json::from_value::<WorkerCodeDescriptorV1>(wire).is_err());
}

#[test]
fn generated_source_manifest_matches_every_system_worker() {
    fn sources(
        directory: &std::path::Path,
        root: &std::path::Path,
        found: &mut BTreeMap<String, String>,
    ) {
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                sources(&path, root, found);
            } else if path.extension().is_some_and(|value| value == "js") {
                let name = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .replace('\\', "/");
                found.insert(
                    name,
                    hex::encode(Sha256::digest(std::fs::read(path).unwrap())),
                );
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/runtime/dist");
    let mut found = BTreeMap::new();
    sources(&root, &root, &mut found);
    assert!(!found.is_empty());
    let manifest: serde_json::Value = serde_json::from_slice(SYSTEM_WORKER_MANIFEST).unwrap();
    assert_eq!(manifest["schemaVersion"], 1);
    assert_eq!(manifest["sources"], json!(found));
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for (name, expected) in manifest["inputs"].as_object().unwrap() {
        let bytes = std::fs::read(repo.join(name)).unwrap();
        assert_eq!(
            expected,
            &json!(hex::encode(Sha256::digest(bytes))),
            "{name}"
        );
    }
}

#[test]
fn platform_module_namespaces_are_reserved_without_binding_exceptions() {
    for name in [
        "cloudflare-internal:open-compute-host-policy",
        "cloudflare-internal:open-compute-forwarding",
        "cloudflare-internal:wrapped-binding",
        "cloudflare-internal:other",
        "open-compute:worker-loader",
        "open-compute:future-module",
    ] {
        let bundle = crate::CanonicalBundle::build(
            name,
            vec![crate::ModuleInput {
                name: name.into(),
                module_type: crate::ModuleType::EsModule,
                bytes: b"export default {}".to_vec(),
            }],
            crate::BundleLimits::default(),
        )
        .unwrap();
        assert_eq!(
            crate::pipeline::validate_injection_module_collisions(bundle.manifest())
                .unwrap_err()
                .code(),
            ErrorCode::BundleInvalid
        );
    }
    let bundle = crate::CanonicalBundle::build(
        "app/index.js",
        vec![crate::ModuleInput {
            name: "app/index.js".into(),
            module_type: crate::ModuleType::EsModule,
            bytes: b"export default {}".to_vec(),
        }],
        crate::BundleLimits::default(),
    )
    .unwrap();
    crate::pipeline::validate_injection_module_collisions(bundle.manifest()).unwrap();
}

#[test]
fn caller_declaration_uses_one_current_capability_and_rejects_selectors() {
    let body = json!({"type":"workflow","id":ResourceId::generate(),"permissions":{"read":true,"write":true},"config":{}});
    let declaration: VersionBindingInput = serde_json::from_value(body.clone()).unwrap();
    let valid = |value: &VersionBindingInput| {
        validate_binding_set(
            &BTreeMap::from([("FLOW".into(), value.clone())]),
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
    };
    valid(&declaration).unwrap();
    assert!(serde_json::to_value(&declaration).unwrap()["capabilityVersion"].is_null());
    for capability in [
        json!(null),
        json!(1),
        json!(2),
        json!("2"),
        json!(2.5),
        json!(-1),
    ] {
        let mut invalid = body.clone();
        invalid["capabilityVersion"] = capability;
        assert!(serde_json::from_value::<VersionBindingInput>(invalid).is_err());
    }
}

#[test]
fn descriptor_subtypes_enforce_their_single_day1_wire_shape() {
    let valid_cache = CachePolicyDescriptorV1 {
        enabled: true,
        cross_version_cache: false,
        entrypoints: BTreeMap::from([(
            "Named_1".to_owned(),
            CacheEntrypointPolicyV1 {
                enabled: true,
                cross_version_cache: true,
            },
        )]),
    };
    valid_cache.validate().unwrap();
    for name in [
        "default".to_owned(),
        "1bad".to_owned(),
        "bad-name".to_owned(),
        "x".repeat(129),
    ] {
        let mut invalid = valid_cache.clone();
        invalid.entrypoints = BTreeMap::from([(
            name,
            CacheEntrypointPolicyV1 {
                enabled: true,
                cross_version_cache: false,
            },
        )]);
        assert_eq!(
            invalid.validate().unwrap_err().code(),
            ErrorCode::EntrypointNotFound
        );
    }

    for kind in [
        BuiltinBindingDescriptorKindV1::VersionMetadata,
        BuiltinBindingDescriptorKindV1::WasmModule,
        BuiltinBindingDescriptorKindV1::TextBlob,
        BuiltinBindingDescriptorKindV1::DataBlob,
    ] {
        let descriptor =
            BuiltinBindingDescriptorV1::new("BUILTIN".to_owned(), kind, Some("tag".to_owned()))
                .unwrap();
        descriptor.sha256().unwrap();
    }
    for (kind, tag) in [
        (BuiltinBindingDescriptorKindV1::Ai, Some("tag".to_owned())),
        (
            BuiltinBindingDescriptorKindV1::Images,
            Some("tag".to_owned()),
        ),
        (
            BuiltinBindingDescriptorKindV1::VersionMetadata,
            Some(String::new()),
        ),
        (
            BuiltinBindingDescriptorKindV1::VersionMetadata,
            Some("bad\nvalue".to_owned()),
        ),
        (
            BuiltinBindingDescriptorKindV1::VersionMetadata,
            Some("x".repeat(1_025)),
        ),
    ] {
        assert!(BuiltinBindingDescriptorV1::new("BUILTIN".to_owned(), kind, tag).is_err());
    }
    let mut builtin =
        BuiltinBindingDescriptorV1::new("AI".to_owned(), BuiltinBindingDescriptorKindV1::Ai, None)
            .unwrap();
    builtin.schema_version = 2;
    assert_eq!(
        builtin.canonical_bytes().unwrap_err().code(),
        ErrorCode::VersionInvariantViolation
    );

    let target = WorkerId::generate();
    for entrypoint in [
        "".to_owned(),
        "1bad".to_owned(),
        "bad-name".to_owned(),
        "x".repeat(129),
    ] {
        assert_eq!(
            ServiceDescriptor::new(
                "SERVICE".to_owned(),
                ServiceTarget::Worker { worker_id: target },
                Some(entrypoint),
                None,
            )
            .unwrap_err()
            .code(),
            ErrorCode::ServiceEntrypointNotFound
        );
    }
    assert!(
        ServiceDescriptor::new(
            "SERVICE".to_owned(),
            ServiceTarget::Worker { worker_id: target },
            None,
            Some(json!([])),
        )
        .is_err()
    );
    assert!(
        ServiceDescriptor::new(
            "S".repeat(65),
            ServiceTarget::Worker { worker_id: target },
            None,
            None,
        )
        .is_err()
    );
    let mut service = ServiceDescriptor::new(
        "SERVICE".to_owned(),
        ServiceTarget::Worker { worker_id: target },
        Some("Named".to_owned()),
        Some(json!({"nested":{"value":true}})),
    )
    .unwrap();
    service.schema_version = 3;
    assert!(service.canonical_bytes().is_err());
    service.schema_version = 1;
    service.policy_version = 2;
    assert!(service.canonical_bytes().is_err());

    let binding = BindingDescriptorV1::new(
        BindingId::generate(),
        "KV".to_owned(),
        BindingKind::KvNamespace,
        ResourceId::generate(),
        1,
        1,
        CanonicalPermissions::default(),
        CanonicalBindingConfig::default(),
    )
    .unwrap();
    binding.sha256().unwrap();
    for (kind, generation, capability) in [
        (BindingKind::KvNamespace, 0, 1),
        (BindingKind::QueueProducer, 1, 1),
        (BindingKind::Workflow, 1, 1),
        (BindingKind::KvNamespace, 1, 2),
    ] {
        assert!(
            BindingDescriptorV1::new(
                BindingId::generate(),
                "KV".to_owned(),
                kind,
                ResourceId::generate(),
                generation,
                capability,
                CanonicalPermissions::default(),
                CanonicalBindingConfig::default(),
            )
            .is_err()
        );
    }
    let mut corrupted_binding = binding.clone();
    corrupted_binding.schema_version = 2;
    assert!(corrupted_binding.canonical_bytes().is_err());

    let queue = QueueProducerBindingDescriptorV1::new(
        BindingId::generate(),
        "QUEUE".to_owned(),
        open_compute_core::QueueId::generate(),
        1,
        1,
    )
    .unwrap();
    queue.sha256().unwrap();
    assert!(
        QueueProducerBindingDescriptorV1::new(
            BindingId::generate(),
            "QUEUE".to_owned(),
            open_compute_core::QueueId::generate(),
            0,
            1,
        )
        .is_err()
    );
    assert!(
        QueueProducerBindingDescriptorV1::new(
            BindingId::generate(),
            "QUEUE".to_owned(),
            open_compute_core::QueueId::generate(),
            1,
            2,
        )
        .is_err()
    );
    for corrupt in 0..4 {
        let mut descriptor = queue.clone();
        match corrupt {
            0 => descriptor.schema_version = 2,
            1 => descriptor.kind = BindingKind::KvNamespace,
            2 => descriptor.capability_version = 2,
            _ => descriptor.queue_lifecycle_generation = 0,
        }
        assert!(descriptor.canonical_bytes().is_err());
    }
}

#[test]
fn worker_descriptor_rejects_invalid_or_conflicting_environment_authority() {
    let account = InstanceId::generate();
    let worker = WorkerId::generate();
    let version = VersionId::generate();
    let bundle = crate::CanonicalBundle::build(
        "index.js",
        vec![crate::ModuleInput {
            name: "index.js".into(),
            module_type: crate::ModuleType::EsModule,
            bytes: b"export default {}".to_vec(),
        }],
        crate::BundleLimits::default(),
    )
    .unwrap();
    let build = |vars: BTreeMap<String, serde_json::Value>,
                 bindings: Vec<BindingDescriptorV1>,
                 queues: Vec<QueueProducerBindingDescriptorV1>,
                 services: Vec<ServiceDescriptor>,
                 cache: CachePolicyDescriptorV1,
                 builtins: Vec<BuiltinBindingDescriptorV1>| {
        WorkerCodeDescriptorV1::new(
            account,
            worker,
            version,
            0,
            "2026-09-08".into(),
            Vec::new(),
            open_compute_storage::worker_repository::EffectiveResourceLimits::standard_defaults(),
            Some((bundle.sha256(), bundle.manifest())),
            None,
            vars,
            Vec::new(),
            bindings,
            queues,
            Vec::new(),
            services,
            cache,
            builtins,
            1,
        )
    };
    assert!(
        WorkerCodeDescriptorV1::new(
            account,
            worker,
            version,
            0,
            "2026-09-08".into(),
            Vec::new(),
            open_compute_storage::worker_repository::EffectiveResourceLimits::standard_defaults(),
            None,
            None,
            BTreeMap::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            CachePolicyDescriptorV1::default(),
            Vec::new(),
            1,
        )
        .is_err()
    );
    assert!(
        WorkerCodeDescriptorV1::new(
            account,
            worker,
            version,
            -1,
            "2026-09-08".into(),
            Vec::new(),
            open_compute_storage::worker_repository::EffectiveResourceLimits::standard_defaults(),
            Some((bundle.sha256(), bundle.manifest())),
            None,
            BTreeMap::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            CachePolicyDescriptorV1::default(),
            Vec::new(),
            1,
        )
        .is_err()
    );

    let binding = BindingDescriptorV1::new(
        BindingId::generate(),
        "DUPLICATE".to_owned(),
        BindingKind::KvNamespace,
        ResourceId::generate(),
        1,
        1,
        CanonicalPermissions::default(),
        CanonicalBindingConfig::default(),
    )
    .unwrap();
    let mut corrupted_binding = binding.clone();
    corrupted_binding.schema_version = 2;
    assert!(
        build(
            BTreeMap::new(),
            vec![corrupted_binding],
            Vec::new(),
            Vec::new(),
            CachePolicyDescriptorV1::default(),
            Vec::new(),
        )
        .is_err()
    );
    let mut duplicate_binding = binding.clone();
    duplicate_binding.binding_id = BindingId::generate();
    duplicate_binding.resource_id = ResourceId::generate();
    assert_eq!(
        build(
            BTreeMap::new(),
            vec![binding.clone(), duplicate_binding],
            Vec::new(),
            Vec::new(),
            CachePolicyDescriptorV1::default(),
            Vec::new(),
        )
        .unwrap_err()
        .code(),
        ErrorCode::BindingTypeMismatch
    );

    let queue = QueueProducerBindingDescriptorV1::new(
        BindingId::generate(),
        "DUPLICATE".to_owned(),
        open_compute_core::QueueId::generate(),
        1,
        1,
    )
    .unwrap();
    assert_eq!(
        build(
            BTreeMap::new(),
            vec![binding],
            vec![queue],
            Vec::new(),
            CachePolicyDescriptorV1::default(),
            Vec::new(),
        )
        .unwrap_err()
        .code(),
        ErrorCode::BindingTypeMismatch
    );
    let service = ServiceDescriptor::new(
        "CONFLICT".to_owned(),
        ServiceTarget::Worker {
            worker_id: target_worker(),
        },
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        build(
            BTreeMap::from([("CONFLICT".to_owned(), json!(true))]),
            Vec::new(),
            Vec::new(),
            vec![service],
            CachePolicyDescriptorV1::default(),
            Vec::new(),
        )
        .unwrap_err()
        .code(),
        ErrorCode::BindingTypeMismatch
    );
    let builtin = BuiltinBindingDescriptorV1::new(
        "CONFLICT".to_owned(),
        BuiltinBindingDescriptorKindV1::Ai,
        None,
    )
    .unwrap();
    assert_eq!(
        build(
            BTreeMap::from([("CONFLICT".to_owned(), json!(true))]),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            CachePolicyDescriptorV1::default(),
            vec![builtin],
        )
        .unwrap_err()
        .code(),
        ErrorCode::BindingTypeMismatch
    );
    let invalid_cache = CachePolicyDescriptorV1 {
        entrypoints: BTreeMap::from([(
            "default".to_owned(),
            CacheEntrypointPolicyV1 {
                enabled: true,
                cross_version_cache: false,
            },
        )]),
        ..Default::default()
    };
    assert!(
        build(
            BTreeMap::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            invalid_cache,
            Vec::new(),
        )
        .is_err()
    );
}

fn target_worker() -> WorkerId {
    WorkerId::generate()
}

fn container_descriptor(worker: bool) -> WorkerCodeDescriptorV1 {
    let bundle = crate::CanonicalBundle::build(
        "index.js",
        vec![crate::ModuleInput {
            name: "index.js".into(),
            module_type: crate::ModuleType::EsModule,
            bytes: b"export default {}".to_vec(),
        }],
        crate::BundleLimits::default(),
    )
    .unwrap();
    let manifest = AssetManifestV1 {
        schema_version: 1,
        entries: vec![crate::assets::AssetEntryV1 {
            path: "/index.html".into(),
            sha256: hex::encode([1; 32]),
            size: 1,
            content_type: "text/html".into(),
        }],
    };
    let routing: AssetRoutingConfigV1 =
        serde_json::from_value(json!({"schemaVersion": 1})).unwrap();
    WorkerCodeDescriptorV1::new(
        InstanceId::generate(),
        WorkerId::generate(),
        VersionId::generate(),
        0,
        "2026-09-08".into(),
        Vec::new(),
        open_compute_storage::worker_repository::EffectiveResourceLimits::standard_defaults(),
        worker.then_some((bundle.sha256(), bundle.manifest())),
        Some((&manifest, &routing)),
        BTreeMap::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        CachePolicyDescriptorV1::default(),
        Vec::new(),
        1,
    )
    .unwrap()
}

#[test]
fn container_associations_are_canonical_and_change_immutable_source_identity() {
    use open_compute_storage::containers::VersionContainerDeclaration;

    let descriptor = container_descriptor(true);
    let original = descriptor.sha256().unwrap();
    assert_eq!(
        descriptor
            .clone()
            .with_containers(Vec::new())
            .unwrap()
            .sha256()
            .unwrap(),
        original
    );
    let first = VersionContainerDeclaration::new(ResourceId::generate(), "first".into()).unwrap();
    let second = VersionContainerDeclaration::new(ResourceId::generate(), "second".into()).unwrap();
    let assigned = descriptor
        .clone()
        .with_containers(vec![first.clone(), second.clone()])
        .unwrap();
    assert_ne!(assigned.sha256().unwrap(), original);
    assert_eq!(
        assigned,
        descriptor
            .clone()
            .with_containers(vec![second, first.clone()])
            .unwrap()
    );
    let changed =
        VersionContainerDeclaration::new(first.namespace_resource_id, "replacement".into())
            .unwrap();
    assert_ne!(
        descriptor
            .clone()
            .with_containers(vec![first.clone()])
            .unwrap()
            .sha256()
            .unwrap(),
        descriptor
            .with_containers(vec![changed])
            .unwrap()
            .sha256()
            .unwrap()
    );
    let named = VersionContainerDeclaration::with_images(
        first.namespace_resource_id,
        first.application_name.clone(),
        BTreeMap::from([
            (
                "blue".into(),
                format!("registry/app@sha256:{}", "1".repeat(64)),
            ),
            (
                "green".into(),
                format!("registry/app@sha256:{}", "2".repeat(64)),
            ),
        ]),
    )
    .unwrap();
    assert_ne!(
        container_descriptor(true)
            .with_containers(vec![first.clone()])
            .unwrap()
            .sha256()
            .unwrap(),
        container_descriptor(true)
            .with_containers(vec![named.clone()])
            .unwrap()
            .sha256()
            .unwrap()
    );
    let mut tampered = named;
    tampered.images.insert(
        "blue".into(),
        format!("registry/app@sha256:{}", "3".repeat(64)),
    );
    assert!(
        container_descriptor(true)
            .with_containers(vec![tampered])
            .is_err()
    );
    let assets = container_descriptor(false);
    assert_eq!(assets.clone().with_containers(Vec::new()).unwrap(), assets);
    assert_eq!(
        assets.with_containers(vec![first]).unwrap_err().code(),
        ErrorCode::VersionInvariantViolation
    );
}

#[test]
fn conflicting_or_corrupted_container_authority_never_produces_a_descriptor() {
    use open_compute_storage::containers::VersionContainerDeclaration;

    let descriptor = container_descriptor(true);
    let original = descriptor.sha256().unwrap();
    let first = VersionContainerDeclaration::new(ResourceId::generate(), "first".into()).unwrap();
    let same_name =
        VersionContainerDeclaration::new(ResourceId::generate(), "first".into()).unwrap();
    let same_namespace =
        VersionContainerDeclaration::new(first.namespace_resource_id, "second".into()).unwrap();
    for declarations in [
        vec![first.clone(), first.clone()],
        vec![first.clone(), same_name],
        vec![first.clone(), same_namespace],
    ] {
        assert_eq!(
            descriptor
                .clone()
                .with_containers(declarations)
                .unwrap_err()
                .code(),
            ErrorCode::VersionInvariantViolation
        );
    }
    let mut wrong_namespace = first.clone();
    wrong_namespace.namespace_resource_id = ResourceId::generate();
    let mut wrong_digest = first.clone();
    wrong_digest.descriptor_sha256[0] ^= 1;
    let mut invalid_name = first;
    invalid_name.application_name = "private-declaration-secret/invalid".into();
    for (declaration, expected_code) in [
        (wrong_namespace, ErrorCode::ResourceInvariantViolation),
        (wrong_digest, ErrorCode::ResourceInvariantViolation),
        (invalid_name, ErrorCode::ResourceInvariantViolation),
    ] {
        let error = descriptor
            .clone()
            .with_containers(vec![declaration])
            .unwrap_err();
        assert_eq!(error.code(), expected_code);
        assert!(!format!("{error:?}").contains("private-declaration-secret"));
    }
    assert_eq!(descriptor.sha256().unwrap(), original);
}
