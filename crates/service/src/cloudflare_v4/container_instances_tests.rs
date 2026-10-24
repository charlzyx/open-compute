use crate::containers::test_support::{Authority, Fixture, authority, fixture, insert_object};
use crate::containers::wire::{Configuration, Disk};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use open_compute_core::{ErrorCode, ResourceId};
use open_compute_storage::containers::{
    ContainerApplicationMetadataPatch, ContainerAuthority, ContainerExecutionSpec, ContainerLease,
    ContainerObjectAuthority, ContainerRunningObservation, ContainerTermination,
};
use serde_json::{Value, json};
use tower::ServiceExt as _;

fn router(fixture: &Fixture) -> Router {
    crate::cloudflare_v4::router(
        fixture.state.clone(),
        crate::cloudflare_v4::storage_router(),
    )
    .with_state(fixture.state.clone())
}

async fn get(router: &Router, path: &str, token: &str) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header(header::AUTHORIZATION, token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap();
    (status, value)
}

fn configure(fixture: &Fixture, authority: &Authority, image: [u8; 32]) {
    let repo = ContainerAuthority::new(fixture.service.storage.db());
    let spec = repo
        .target(fixture.service.instance_id(), authority.app.id, 1)
        .unwrap()
        .spec;
    let configuration = Configuration {
        image: format!(
            "registry.example.test/{}/authority-app@sha256:{}",
            fixture.service.instance_id(),
            hex::encode(image)
        ),
        instance_type: None,
        vcpu: Some(spec.resources.cpu_nanos as f64 / 1e9),
        memory_mib: Some(spec.resources.memory_bytes / (1 << 20)),
        disk: Some(Disk {
            size_mb: spec.resources.disk_bytes as f64 / 1e6,
        }),
    };
    assert_eq!(
        configuration.resources(spec.resources).unwrap(),
        spec.resources
    );
    repo.commit_application_configuration(
        fixture.service.instance_id(),
        &authority.app,
        &spec,
        &serde_json::to_vec(&configuration).unwrap(),
        ContainerApplicationMetadataPatch {
            max_instances: Some(4),
            observability_logs: Some(false),
        },
        9,
    )
    .unwrap();
}

fn execution(
    fixture: &Fixture,
    authority: &ContainerObjectAuthority,
    key: &str,
) -> ContainerExecutionSpec {
    let repo = ContainerAuthority::new(fixture.service.storage.db());
    let application = repo
        .application_for_namespace(
            fixture.service.instance_id(),
            authority.namespace_resource_id,
            "authority-app",
        )
        .unwrap();
    let target = repo
        .target(
            fixture.service.instance_id(),
            authority.application_id,
            application.target_generation.unwrap(),
        )
        .unwrap()
        .spec;
    let generation = repo
        .application_leases(fixture.service.instance_id(), authority.application_id)
        .unwrap()
        .iter()
        .filter(|lease| lease.identity.object_id == authority.object_id)
        .map(|lease| lease.identity.lease_generation)
        .max()
        .unwrap_or(0)
        + 1;
    let owner = open_compute_storage::containers::ContainerLeaseIdentity {
        namespace_resource_id: authority.namespace_resource_id,
        object_id: authority.object_id,
        object_generation: authority.object_generation,
        lease_generation: generation,
    }
    .owner_key(fixture.service.instance_id());
    let options =
        json!({"enableInternet":false,"entrypoint":null,"environmentVariables":[],"labels":[]});
    let envelope = fixture
        .service
        .storage
        .crypto()
        .seal_container_startup(
            &open_compute_core::SecretBytes::new(serde_json::to_vec(&options).unwrap()),
            fixture.service.instance_id(),
            &owner,
            key,
        )
        .unwrap();
    ContainerExecutionSpec {
        directory_snapshots: Vec::new(),
        image_sha256: target.image_sha256.unwrap(),
        resources: target.resources,
        enable_internet: false,
        startup: open_compute_storage::containers::ContainerLeaseStartup {
            image_reference: format!(
                "registry.example.test/{}/authority-app@sha256:{}",
                fixture.service.instance_id(),
                hex::encode(target.image_sha256.unwrap())
            ),
            ..Default::default()
        },
        startup_envelope: serde_json::to_vec(&envelope).unwrap(),
    }
}

fn running(fixture: &Fixture, lease: &ContainerLease, now: i64) {
    let repo = ContainerAuthority::new(fixture.service.storage.db());
    let target = repo
        .target(
            fixture.service.instance_id(),
            lease.application_id,
            lease.target_generation,
        )
        .unwrap();
    repo.observe_running(
        lease.identity,
        &ContainerRunningObservation {
            opaque_engine_key: &lease.opaque_engine_key,
            start_identity: &"a".repeat(64),
            image_sha256: &lease.effective_image_sha256,
            provider_contract_sha256: &target.spec.provider_contract_sha256,
        },
        now,
    )
    .unwrap();
}

fn stopped(fixture: &Fixture, lease: &ContainerLease, now: i64) {
    let repo = ContainerAuthority::new(fixture.service.storage.db());
    let current = repo
        .lease(fixture.service.instance_id(), lease.identity)
        .unwrap();
    repo.begin_stop(lease.identity, &lease.opaque_engine_key, now)
        .unwrap();
    repo.finish_stopped(
        lease.identity,
        &ContainerTermination {
            opaque_engine_key: &lease.opaque_engine_key,
            start_identity: current.start_identity.as_deref(),
            terminated: true,
            cleanup_complete: true,
            late_operations_fenced: true,
        },
        now + 1,
    )
    .unwrap();
}

async fn expect_instance(router: &Router, path: &str, state: &str) -> Value {
    let (status, response) = get(router, path, "Bearer read-token").await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["success"], true);
    assert_eq!(response["result"]["status"]["state"], state);
    let rendered = serde_json::to_string(&response).unwrap();
    for private in [
        "opaqueEngineKey",
        "startIdentity",
        "providerContract",
        "startup",
        "ownerKey",
        "Token",
    ] {
        assert!(
            !rendered.contains(private),
            "public response exposes {private}"
        );
    }
    response["result"].clone()
}

#[tokio::test]
async fn container_instances_observe_durable_states_and_select_latest_effective_execution() {
    let fixture = fixture().await;
    let image = fixture.admit_image("public-instances-original").await;
    let authority = authority(&fixture.service, &fixture._artifacts, image, true);
    configure(&fixture, &authority, image);
    let application = router(&fixture);
    let base = format!(
        "/accounts/{}/containers/applications/{}",
        fixture.service.instance_id(),
        authority.app.id
    );
    let path = format!("{base}/instances/{}", authority.object.object_id);
    let repo = ContainerAuthority::new(fixture.service.storage.db());
    let first = repo
        .reserve(
            &authority.object,
            &"1".repeat(64),
            &execution(&fixture, &authority.object, &"1".repeat(64)),
            4,
            10,
        )
        .unwrap();
    let admitted = expect_instance(&application, &path, "provisioning").await;
    assert_eq!(admitted["id"], authority.object.object_id.to_string());
    assert_eq!(admitted["application_id"], authority.app.id.to_string());
    assert_eq!(
        admitted["image"],
        format!(
            "registry.example.test/{}/authority-app@sha256:{}",
            fixture.service.instance_id(),
            hex::encode(image)
        )
    );
    repo.mark_starting(first.identity, &first.opaque_engine_key, 20)
        .unwrap();
    let starting = expect_instance(&application, &path, "provisioning").await;
    assert_ne!(
        admitted["status"]["updated_at"],
        starting["status"]["updated_at"]
    );
    running(&fixture, &first, 30);
    expect_instance(&application, &path, "running").await;
    repo.begin_stop(first.identity, &first.opaque_engine_key, 40)
        .unwrap();
    expect_instance(&application, &path, "stopping").await;
    stopped(&fixture, &first, 50);
    expect_instance(&application, &path, "stopped").await;

    let replacement = fixture.admit_image("public-instances-override").await;
    let mut resources = first.effective_resources;
    resources.cpu_nanos /= 2;
    resources.memory_bytes /= 2;
    resources.disk_bytes /= 2;
    let replacement_reference = format!(
        "registry.example.test/{}/authority-app@sha256:{}",
        fixture.service.instance_id(),
        hex::encode(replacement)
    );
    let mut forbidden = execution(&fixture, &authority.object, &"2".repeat(64));
    forbidden.image_sha256 = replacement;
    forbidden.startup.image_reference = replacement_reference.clone();
    forbidden.resources = resources;
    assert_eq!(
        repo.reserve(&authority.object, &"2".repeat(64), &forbidden, 4, 60)
            .unwrap_err()
            .code(),
        ErrorCode::ResourceInvariantViolation
    );
    let mut target = repo
        .target(
            fixture.service.instance_id(),
            authority.app.id,
            first.target_generation,
        )
        .unwrap()
        .spec;
    target.image_sha256 = Some(replacement);
    target.resources = resources;
    let configuration = Configuration {
        image: replacement_reference.clone(),
        instance_type: None,
        vcpu: Some(resources.cpu_nanos as f64 / 1e9),
        memory_mib: Some(resources.memory_bytes / (1 << 20)),
        disk: Some(Disk {
            size_mb: resources.disk_bytes as f64 / 1e6,
        }),
    };
    repo.commit_application_configuration(
        fixture.service.instance_id(),
        &authority.app,
        &target,
        &serde_json::to_vec(&configuration).unwrap(),
        ContainerApplicationMetadataPatch {
            max_instances: None,
            observability_logs: None,
        },
        60,
    )
    .unwrap();
    let execution = execution(&fixture, &authority.object, &"2".repeat(64));
    let second = repo
        .reserve(&authority.object, &"2".repeat(64), &execution, 4, 60)
        .unwrap();
    assert_eq!(
        second.identity.lease_generation,
        first.identity.lease_generation + 1
    );
    repo.mark_starting(second.identity, &second.opaque_engine_key, 61)
        .unwrap();
    repo.mark_lost(second.identity, &second.opaque_engine_key, 62)
        .unwrap();
    let current = expect_instance(&application, &path, "unknown").await;
    assert_eq!(current["image"], replacement_reference);
    assert_eq!(
        current["configuration"],
        json!({"vcpu":resources.cpu_nanos as f64/1e9,"memory":resources.memory_bytes/(1<<20),"disk":resources.disk_bytes as f64/1e6})
    );
    let (status, listed) = get(
        &application,
        &format!("{base}/instances-v2"),
        "Bearer read-token",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["result"], json!([current]));
    assert_eq!(
        repo.application_leases(fixture.service.instance_id(), authority.app.id)
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn container_instances_filter_page_and_reject_invalid_scope_and_queries() {
    let fixture = fixture().await;
    let image = fixture.admit_image("public-instance-filters").await;
    let authority = authority(&fixture.service, &fixture._artifacts, image, true);
    configure(&fixture, &authority, image);
    let application = router(&fixture);
    let base = format!(
        "/accounts/{}/containers/applications/{}",
        fixture.service.instance_id(),
        authority.app.id
    );
    let list = format!("{base}/instances-v2");
    let repo = ContainerAuthority::new(fixture.service.storage.db());
    let first = repo
        .reserve(
            &authority.object,
            &"3".repeat(64),
            &execution(&fixture, &authority.object, &"3".repeat(64)),
            4,
            10,
        )
        .unwrap();
    let mut objects = vec![authority.object.object_id.to_string()];
    for fill in [8, 9] {
        let object_id = insert_object(
            &fixture.service,
            authority.object.namespace_resource_id,
            fill,
        );
        let object = ContainerObjectAuthority {
            object_id,
            ..authority.object
        };
        let lease = repo
            .reserve(
                &object,
                &format!("{fill:064x}"),
                &execution(&fixture, &object, &format!("{fill:064x}")),
                4,
                11,
            )
            .unwrap();
        objects.push(object_id.to_string());
        repo.mark_starting(lease.identity, &lease.opaque_engine_key, 12)
            .unwrap();
        if fill == 8 {
            running(&fixture, &lease, 13);
        } else {
            stopped(&fixture, &lease, 14);
        }
    }
    objects.sort();
    let (status, first_page) = get(
        &application,
        &format!("{list}?per_page=1"),
        "Bearer read-token",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first_page["result"].as_array().unwrap().len(), 1);
    assert_eq!(first_page["result"][0]["id"], objects[0]);
    let cursor = first_page["result_info"]["next_page_token"]
        .as_str()
        .unwrap();
    assert!(!cursor.is_empty());
    let (status, second_page) = get(
        &application,
        &format!("{list}?per_page=2&page_token={cursor}"),
        "Bearer read-token",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        second_page["result"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        objects[1..]
    );
    assert_eq!(second_page["result_info"]["next_page_token"], "");
    for (query, count) in [
        ("state=active", 2),
        ("state=not-active", 1),
        ("name_prefix=absent", 0),
    ] {
        let (status, page) = get(
            &application,
            &format!("{list}?{query}"),
            "Bearer read-token",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(page["result"].as_array().unwrap().len(), count);
    }
    let (status, named) = get(
        &application,
        &format!("{list}?name_prefix={}", first.identity.object_id),
        "Bearer read-token",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(named["result"].as_array().unwrap().len(), 1);
    for query in [
        "state=running",
        "unknown=1",
        "per_page=0",
        "per_page=1001",
        "per_page=invalid",
        "page_token=!",
        "page_token=OTk",
        "per_page=1&per_page=2",
    ] {
        let (status, error) = get(
            &application,
            &format!("{list}?{query}"),
            "Bearer read-token",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {error}");
        assert_eq!(error["success"], false);
    }
    for path in [
        format!("{base}/instances/missing"),
        format!(
            "/accounts/{}/containers/applications/{}/instances-v2",
            fixture.service.instance_id(),
            ResourceId::generate()
        ),
    ] {
        assert_eq!(
            get(&application, &path, "Bearer read-token").await.0,
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        get(
            &application,
            &format!("{base}/instances/{}?state=active", first.identity.object_id),
            "Bearer read-token"
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        get(&application, &list, "Bearer invalid-token").await.0,
        StatusCode::UNAUTHORIZED
    );
}
