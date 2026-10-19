//! Real pinned workerd, ordinary Containers SDK, qualified Broker and immutable rollout Gate.

#![cfg(feature = "test-support")]

mod cf;
mod cleanup;
mod exec_options;
mod http;
mod images;
mod inactivity;
mod named_images;
#[allow(
    dead_code,
    reason = "reuse existing daemon ownership and recovery fixture"
)]
#[path = "../workflow_support/platform_process.rs"]
mod platform_process;
mod signals;
mod snapshots;

use open_compute_artifacts::MockS3;
use open_compute_core::{
    InstanceId, PlatformConfig, ResourceId, SystemClock,
    config::{CacheConfig, ContainersConfig, DataConfig},
    containers::ContainerLeaseState,
};
use open_compute_service::container_broker::QualifiedContainerProvider;
use open_compute_storage::{
    PlatformStorage,
    containers::{ContainerAuthority, ContainerLease},
    control_db::ControlDb,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Inputs {
    containers: ContainersConfig,
    images: [images::Image; 2],
}

#[derive(Serialize)]
struct ContainerConfig<'a> {
    containers: &'a ContainersConfig,
    cache: CacheConfig,
}

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_owned()
}

fn inputs() -> Inputs {
    let path = std::env::var_os("OPEN_COMPUTE_TEST_CONTAINERS_INPUT")
        .expect("P23 requires explicit qualified Linux Broker and two prepared OCI image inputs");
    let path = PathBuf::from(path);
    assert!(
        path.is_absolute(),
        "Container Gate inputs must be an absolute private path"
    );
    assert!(fs::metadata(&path).unwrap().len() <= 65536);
    let input: Inputs = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    input.containers.validate().unwrap();
    assert!(input.containers.max_instances >= 2);
    input
}

fn leases(
    data: &std::path::Path,
    instance: InstanceId,
    application: ResourceId,
) -> Vec<ContainerLease> {
    let db = ControlDb::open_readonly_wal_aware(&data.join("control.sqlite"), 1000).unwrap();
    ContainerAuthority::new(&db)
        .application_leases(instance, application)
        .unwrap()
}

async fn tenant(client: &http::Client, public: &str, host: &str, path: &str) -> Value {
    http::json(client, public, host, path, "GET", None, Value::Null).await
}

async fn wait_running(
    data: &std::path::Path,
    instance: InstanceId,
    application: ResourceId,
    target: u64,
) -> ContainerLease {
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            if let Some(lease) = leases(data, instance, application)
                .into_iter()
                .find(|lease| {
                    lease.target_generation == target && lease.state == ContainerLeaseState::Running
                })
            {
                return lease;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("Provider must positively observe the real main process Running")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordinary_sdk_native_execution_code_deploy_rollout_and_restart_use_real_containers() {
    let inputs = inputs();
    let provider = QualifiedContainerProvider::preflight(&inputs.containers).await
        .expect("P23 requires active qualified Linux Docker; protocol mocks and OrbStack quota are not evidence");
    provider.ensure_active().await.unwrap();
    let purpose = repo().join(".temp/p23-product");
    fs::create_dir_all(&purpose).unwrap();
    let directory = tempfile::Builder::new()
        .prefix("run-")
        .tempdir_in(&purpose)
        .unwrap();
    let root = directory.path().to_owned();
    let _evidence = platform_process::Evidence(Some(directory));
    let data = root.join("data");
    let storage = PlatformStorage::bootstrap(
        &DataConfig {
            path: data.clone(),
            master_key_file: data.join("keys/master.key"),
            ..Default::default()
        },
        &SystemClock,
    )
    .unwrap();
    let instance = storage.identity().instance_id;
    drop(storage);
    let _cleanup = cleanup::Cleanup {
        data: data.clone(),
        instance,
        provider: inputs.containers.clone(),
    };
    let mock = MockS3::spawn("open-compute").await;
    let (public, admin) = platform_process::distinct_addresses();
    let config = platform_process::config(&root, &data, &mock.endpoint, public, admin);
    let mut text = fs::read_to_string(&config).unwrap();
    text.push('\n');
    text.push_str(
        &toml::to_string(&ContainerConfig {
            containers: &inputs.containers,
            cache: CacheConfig {
                max_bytes: 1 << 30,
                max_artifact_bytes: 1 << 30,
                ..Default::default()
            },
        })
        .unwrap(),
    );
    PlatformConfig::from_toml_str(&text).unwrap();
    fs::write(&config, text).unwrap();
    let log = root.join("ocd.stderr.log");
    let mut process = platform_process::spawn(&config, &log);
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build_http();
    platform_process::ready(&client, admin, &mut process).await;
    let admin = admin.to_string();
    let public = public.to_string();
    let accounts = http::json(
        &client,
        &admin,
        &admin,
        "/client/v4/accounts",
        "GET",
        Some("Bearer workflow-read-only"),
        Value::Null,
    )
    .await;
    let account = accounts["result"][0]["id"].as_str().unwrap();
    let account_path = format!("/client/v4/accounts/{account}");
    let image = inputs.images[0].push(&client, &admin, account).await;
    let replacement_image = inputs.images[1].push(&client, &admin, account).await;
    assert_ne!(image, replacement_image);
    let state = root.join("cf-state");
    let project = cf::project(&root);
    cf::deploy(
        &project,
        &state,
        &admin,
        account,
        "one",
        &image,
        [&image, &replacement_image],
    )
    .await;
    let uploaded = cf::active_version(&data, instance).to_string();
    let version = http::json(
        &client,
        &admin,
        &admin,
        &format!("{account_path}/workers/scripts/containers-fixture/versions/{uploaded}"),
        "GET",
        Some("Bearer workflow-read-only"),
        Value::Null,
    )
    .await;
    assert_eq!(version["result"]["id"], uploaded);
    let bindings = version["result"]["resources"]["bindings"]
        .as_array()
        .unwrap();
    let namespaces = bindings
        .iter()
        .filter(|binding| {
            binding["type"] == "durable_object_namespace"
                && binding["class_name"] == "FixtureContainer"
                && binding
                    .get("script_name")
                    .is_none_or(|script| script == "containers-fixture")
        })
        .collect::<Vec<_>>();
    assert_eq!(namespaces.len(), 1);
    let namespace = namespaces[0]["namespace_id"].as_str().unwrap();
    let idle_namespaces = bindings
        .iter()
        .filter(|binding| {
            binding["type"] == "durable_object_namespace"
                && binding["class_name"] == "RawIdleContainer"
                && binding
                    .get("script_name")
                    .is_none_or(|script| script == "containers-fixture")
        })
        .collect::<Vec<_>>();
    assert_eq!(idle_namespaces.len(), 1);
    let idle_namespace = idle_namespaces[0]["namespace_id"].as_str().unwrap();
    assert_ne!(namespace, idle_namespace);
    let named_namespaces = bindings
        .iter()
        .filter(|binding| {
            binding["type"] == "durable_object_namespace"
                && binding["class_name"] == "NamedImageContainer"
                && binding
                    .get("script_name")
                    .is_none_or(|script| script == "containers-fixture")
        })
        .collect::<Vec<_>>();
    assert_eq!(named_namespaces.len(), 1);
    let named_namespace = named_namespaces[0]["namespace_id"].as_str().unwrap();
    assert_ne!(namespace, named_namespace);
    assert_ne!(idle_namespace, named_namespace);
    http::cf(
        &repo(),
        &state,
        &admin,
        account,
        &["containers", "applications", "list"],
    )
    .await;
    let applications = http::json(
        &client,
        &admin,
        &admin,
        &format!("{account_path}/containers/applications"),
        "GET",
        Some("Bearer workflow-read-only"),
        Value::Null,
    )
    .await;
    let applications = applications["result"].as_array().unwrap();
    assert_eq!(applications.len(), 3);
    let initial = applications
        .iter()
        .find(|app| app["name"] == "containers-fixture")
        .unwrap();
    let idle = applications
        .iter()
        .find(|app| app["name"] == "containers-idle-fixture")
        .unwrap();
    let named = applications
        .iter()
        .find(|app| app["name"] == "containers-named-fixture")
        .unwrap();
    assert_eq!(named["scheduling_policy"], "durable_object");
    assert_eq!(named["id"], named_namespace);
    assert_eq!(named["durable_objects"]["namespace_id"], named_namespace);
    for key in ["configuration", "max_instances"] {
        assert!(named.get(key).is_none());
    }
    let named_id = named["id"].as_str().unwrap();
    let named_application = named_images::application(&data, instance);
    assert_eq!(idle["configuration"]["image"], image);
    assert_eq!(idle["durable_objects"]["namespace_id"], idle_namespace);
    assert_eq!(idle["rollout_active_grace_period"], 0);
    assert_eq!(idle["max_instances"], 2);
    let idle_id = idle["id"].as_str().unwrap();
    let idle_application: ResourceId = idle_id.parse().unwrap();
    assert_eq!(initial["configuration"]["image"], image);
    assert_eq!(initial["configuration"]["vcpu"], 0.0625);
    assert_eq!(initial["configuration"]["memory_mib"], 256);
    assert_eq!(initial["configuration"]["disk"]["size_mb"], 2000.0);
    assert_eq!(initial["durable_objects"]["namespace_id"], namespace);
    assert_eq!(initial["rollout_active_grace_period"], 0);
    assert_eq!(initial["max_instances"], 2);
    let id = initial["id"].as_str().unwrap();
    let application: ResourceId = id.parse().unwrap();
    let base = format!("{account_path}/containers/applications/{id}");
    let host = format!("containers-fixture.{instance}.localhost");
    let first = tenant(&client, &public, &host, "/").await;
    assert_eq!(first["image"], "one");
    assert_eq!(first["code"], "one");
    assert_eq!(
        first["exec"],
        "exec-ok",
        "default exit={:?}, stderr bytes={:?}, assertion={:?}; override exit={:?}, stdout bytes={:?}; egress exit={:?}, stdout bytes={:?}",
        first["exitCode"].as_i64(),
        first["execStderrBytes"].as_u64(),
        first["execStderrHasAssertion"].as_bool(),
        first["execOverrideExitCode"].as_i64(),
        first["execOverride"].as_str().map(str::len),
        first["egressExitCode"].as_i64(),
        first["egress"].as_str().map(str::len)
    );
    assert_eq!(first["exitCode"], 0);
    assert_eq!(first["execOverride"], "explicit-ok");
    assert_eq!(first["execOverrideExitCode"], 0);
    assert_eq!(first["egress"], "one");
    assert_eq!(first["egressExitCode"], 0);
    assert_eq!(first["running"], true);
    assert_eq!(first["visits"], 1);
    assert_eq!(
        first["envKeys"],
        json!(["CODE_REVISION", "IDLE_OBJECTS", "NAMED_OBJECTS", "OBJECTS"])
    );
    assert_eq!(
        tenant(&client, &public, &host, "/inspection").await,
        json!({
            "running": true,
            "info": {"image": image, "labels": {"purpose": "containers-fixture"}},
            "images": {},
        })
    );
    let old = wait_running(&data, instance, application, 1).await;
    let proof = provider.observe(&old.opaque_engine_key).await.unwrap();
    assert!(proof.running && !proof.terminated);
    assert_eq!(
        old.start_identity.as_deref(),
        Some(proof.start_identity.as_str())
    );
    assert_eq!(
        old.effective_resources,
        open_compute_core::containers::ContainerInstanceProfile::Lite
            .resources(inputs.containers.resources)
            .unwrap()
    );
    // Cross the ordinary Durable Object idle window while the SDK keeps the container alive.
    tokio::time::sleep(Duration::from_secs(12)).await;
    let after_idle = tenant(&client, &public, &host, "/").await;
    assert_eq!(after_idle["image"], "one");
    assert_eq!(after_idle["code"], "one");
    assert_eq!(after_idle["visits"], 2);
    assert_eq!(after_idle["exec"], "exec-ok");
    assert_eq!(after_idle["egress"], "one");
    assert_eq!(after_idle["egressExitCode"], 0);
    let idle_lease = wait_running(&data, instance, application, 1).await;
    assert_eq!(idle_lease.identity, old.identity);
    assert_eq!(idle_lease.start_identity, old.start_identity);
    let idle_proof = provider.observe(&old.opaque_engine_key).await.unwrap();
    assert!(idle_proof.running);
    assert_eq!(idle_proof.start_identity, proof.start_identity);
    let instances = http::json(
        &client,
        &admin,
        &admin,
        &format!("{base}/instances-v2?per_page=1"),
        "GET",
        Some("Bearer workflow-read-only"),
        Value::Null,
    )
    .await;
    assert_eq!(instances["result"].as_array().unwrap().len(), 1);
    assert_eq!(instances["result"][0]["status"]["state"], "running");
    let public_id = instances["result"][0]["id"].as_str().unwrap();
    let single = http::json(
        &client,
        &admin,
        &admin,
        &format!("{base}/instances/{public_id}"),
        "GET",
        Some("Bearer workflow-read-only"),
        Value::Null,
    )
    .await;
    assert_eq!(single["result"]["id"], public_id);
    let (denied, _, _) = http::request(
        &client,
        &admin,
        &admin,
        &base,
        "PATCH",
        Some("Bearer workflow-read-only"),
        "application/json",
        serde_json::to_vec(&json!({"configuration":initial["configuration"],"max_instances":2}))
            .unwrap(),
    )
    .await;
    assert_eq!(denied, 403);
    let streams = tenant(&client, &public, &host, "/streams").await;
    assert_eq!(
        streams,
        json!({"stdout":"streamed-input","stderr":"stderr","exitCode":7,"pty":true,"ptyInitial":"initial:80x24\r\n","ptyStdout":"resized:100x30\r\npty-stderr\r\n","ptyStderrAbsent":true,"ptyExitCode":0,"ptyEnvironment":"pty-env-ok","ptyEnvironmentExitCode":0})
    );
    assert_eq!(
        tenant(&client, &public, &host, "/cancel").await,
        json!({"exitCode":137,"preAbortName":"AbortError","probe":"alive"})
    );
    assert_eq!(
        tenant(&client, &public, &host, "/network-lifecycle").await,
        json!({
            "https":{"status":201,"header":"tls-ok","body":"https:one:tls-body","certificateVerified":true,"closed":true},
            "websocket":{"header":"websocket-ok","body":"echo:websocket-body","closeCode":1000,"closeReason":"finished","wasClean":true,"exitCode":0},
            "http":{"matched":"http-specific","unmatched":"http-fallback","closed":true}
        })
    );
    assert_eq!(
        tenant(
            &client,
            &public,
            &host,
            "/exec-lifecycle?object=exec-lifecycle",
        )
        .await,
        json!({
            "pidMatches":true,
            "late":{"stdout":"late-out","stderr":"late-err","exitCode":7},
            "truncated":{"outputInterrupted":true,"exitCode":7},
            "shellless":{"stdout":"shellless-readonly","exitCode":0},
            "ownerLoss":{"outputInterrupted":true,"running":false}
        })
    );
    let independent = leases(&data, instance, application)
        .into_iter()
        .filter(|lease| lease.identity.object_id != old.identity.object_id)
        .collect::<Vec<_>>();
    assert_eq!(independent.len(), 1);
    assert_eq!(independent[0].state, ContainerLeaseState::Stopped);
    let independent_proof = provider
        .observe(&independent[0].opaque_engine_key)
        .await
        .unwrap();
    assert!(
        independent_proof.terminated
            && independent_proof.cleanup_complete
            && independent_proof.late_operations_fenced
    );
    cf::update_code(&project);
    cf::deploy(
        &project,
        &state,
        &admin,
        account,
        "two",
        &image,
        [&image, &replacement_image],
    )
    .await;
    assert_ne!(cf::active_version(&data, instance).to_string(), uploaded);
    assert_eq!(
        ContainerAuthority::new(
            &ControlDb::open_readonly_wal_aware(&data.join("control.sqlite"), 1000).unwrap()
        )
        .rollouts(instance, application)
        .unwrap()
        .len(),
        0,
        "code-only cf deploy must not manufacture a Container rollout"
    );
    let code_only = tenant(&client, &public, &host, "/").await;
    assert_eq!(code_only["image"], "one");
    assert_eq!(code_only["code"], "two");
    assert_eq!(code_only["visits"], 3);
    assert_eq!(code_only["egress"], "two");
    assert_eq!(code_only["egressExitCode"], 0);
    let still_old = wait_running(&data, instance, application, 1).await;
    assert_eq!(still_old.identity, old.identity);
    assert_eq!(still_old.start_identity, old.start_identity);
    cf::deploy(
        &project,
        &state,
        &admin,
        account,
        "two",
        &replacement_image,
        [&image, &replacement_image],
    )
    .await;
    let updated = http::json(
        &client,
        &admin,
        &admin,
        &base,
        "GET",
        Some("Bearer workflow-read-only"),
        Value::Null,
    )
    .await;
    assert_eq!(updated["result"]["version"], 2);
    assert_eq!(
        updated["result"]["configuration"]["image"],
        replacement_image
    );
    let rollouts = ContainerAuthority::new(
        &ControlDb::open_readonly_wal_aware(&data.join("control.sqlite"), 1000).unwrap(),
    )
    .rollouts(instance, application)
    .unwrap();
    assert_eq!(
        rollouts.len(),
        1,
        "cf deploy must create the changed-image rollout"
    );
    assert_eq!(rollouts[0].target_generation, 2);
    assert_eq!(rollouts[0].steps, vec![100]);
    let new = wait_running(&data, instance, application, 2).await;
    assert_eq!(new.identity.object_id, old.identity.object_id);
    assert_eq!(
        new.identity.object_generation,
        old.identity.object_generation
    );
    assert!(new.identity.lease_generation > old.identity.lease_generation);
    assert_ne!(new.opaque_engine_key, old.opaque_engine_key);
    assert_ne!(new.start_identity, old.start_identity);
    let mut replacement_startup = old.startup.clone();
    replacement_startup.image_reference = replacement_image.clone();
    assert_eq!(new.startup, replacement_startup);
    let old_after = leases(&data, instance, application)
        .into_iter()
        .find(|lease| lease.identity == old.identity)
        .unwrap();
    assert_eq!(old_after.state, ContainerLeaseState::Stopped);
    let old_proof = provider.observe(&old.opaque_engine_key).await.unwrap();
    assert!(old_proof.terminated && old_proof.cleanup_complete && old_proof.late_operations_fenced);
    let after_rollout = tenant(&client, &public, &host, "/").await;
    assert_eq!(after_rollout["image"], "two");
    assert_eq!(after_rollout["code"], "two");
    assert_eq!(after_rollout["visits"], 4);
    assert_eq!(after_rollout["exec"], "exec-ok");
    assert_eq!(after_rollout["egress"], "two");
    assert_eq!(after_rollout["egressExitCode"], 0);
    assert_eq!(
        tenant(&client, &public, &host, "/inspection").await["info"]["image"],
        replacement_image
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let completed = http::json(
                &client,
                &admin,
                &admin,
                &format!("{base}/rollouts"),
                "GET",
                Some("Bearer workflow-read-only"),
                Value::Null,
            )
            .await;
            if completed["result"][0]["status"] == "completed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("actual replacement Running proof must complete the persisted rollout");
    process.stop().await;
    process.restart(&config, &log);
    platform_process::ready(&client, admin.parse().unwrap(), &mut process).await;
    let restarted = tenant(&client, &public, &host, "/").await;
    assert_eq!(restarted["image"], "two");
    assert_eq!(restarted["visits"], 5);
    assert_eq!(restarted["exec"], "exec-ok");
    assert_eq!(restarted["code"], "two");
    assert_eq!(restarted["egress"], "two");
    assert_eq!(restarted["egressExitCode"], 0);
    let current = wait_running(&data, instance, application, 2).await;
    assert_eq!(current.identity, new.identity);
    assert_eq!(current.start_identity, new.start_identity);
    assert_eq!(
        tenant(&client, &public, &host, "/stop").await["running"],
        false
    );
    assert_eq!(
        tenant(&client, &public, &host, "/inspection").await,
        json!({"running": false, "info": null, "images": {}})
    );
    let final_state = leases(&data, instance, application);
    assert!(
        final_state
            .iter()
            .all(|lease| lease.state == ContainerLeaseState::Stopped)
    );
    let empty_instances = http::json(
        &client,
        &admin,
        &admin,
        &format!("{base}/instances-v2?per_page=1"),
        "GET",
        Some("Bearer workflow-read-only"),
        Value::Null,
    )
    .await;
    assert!(
        empty_instances["result"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["status"]["state"] != "running")
    );
    let stopped = provider.observe(&new.opaque_engine_key).await.unwrap();
    assert!(stopped.terminated && stopped.cleanup_complete && stopped.late_operations_fenced);
    inactivity::natural_idle_stops_subject_and_preserves_control(
        &client,
        &public,
        &host,
        &data,
        instance,
        idle_application,
        &provider,
    )
    .await;
    named_images::lifecycle(
        &client,
        &public,
        &host,
        &data,
        (instance, named_application),
        &provider,
        [&image, &replacement_image],
    )
    .await;
    signals::lifecycle(&client, &public, &host).await;
    exec_options::lifecycle(&client, &public, &host).await;
    named_images::assert_stopped(&data, instance, named_application, &provider).await;
    let saved = snapshots::capture_and_cross_restore(
        &client,
        &public,
        &host,
        &config,
        instance,
        named_application,
        &provider,
    )
    .await;
    let cleanup_image = ContainerAuthority::new(
        &ControlDb::open_readonly_wal_aware(&data.join("control.sqlite"), 1000).unwrap(),
    )
    .image(instance, &saved.image)
    .unwrap();
    process.stop().await;
    process.restart(&config, &log);
    platform_process::ready(&client, admin.parse().unwrap(), &mut process).await;
    snapshots::restore_after_restart(
        &client,
        &public,
        &host,
        &config,
        instance,
        named_application,
        saved,
    )
    .await;
    named_images::assert_images(&client, &public, &host, [&image, &replacement_image]).await;
    for application_path in [
        base,
        format!("{account_path}/containers/applications/{idle_id}"),
        format!("{account_path}/containers/applications/{named_id}"),
    ] {
        http::json(
            &client,
            &admin,
            &admin,
            &application_path,
            "DELETE",
            Some("Bearer workflow-deployer"),
            Value::Null,
        )
        .await;
    }
    let diagnostics = fs::read_to_string(&log).unwrap();
    for secret in [
        "workflow-admin",
        "workflow-deployer",
        "workflow-read-only",
        "p23-owned-startup",
        &old.opaque_engine_key,
        &new.opaque_engine_key,
    ] {
        assert!(
            !diagnostics.contains(secret),
            "private Container authority leaked in daemon logs"
        );
    }
    process.stop().await;
    eprintln!(
        "p23-owned-image-cleanup={}",
        json!({
            "config": format!("sha256:{}", hex::encode(cleanup_image.config_sha256)),
            "diffIds": cleanup_image.diff_ids.iter()
                .map(|id| format!("sha256:{}", hex::encode(id)))
                .collect::<Vec<_>>()
        })
    );
}
