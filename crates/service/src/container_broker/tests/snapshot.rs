//! Capture is process-bound, bounded and permanently fenced after uncertain engine writes.

use super::*;
use std::io::Read as _;

#[tokio::test]
async fn snapshot_qualification_proves_export_semantics_and_cleans_every_failed_probe() {
    for failure in [
        None,
        Some("volatile"),
        Some("content"),
        Some("mode"),
        Some("identity"),
        Some("pause"),
        Some("export"),
        Some("unpause"),
    ] {
        let broker = RestartBroker::new().await;
        if matches!(failure, Some("pause" | "export" | "unpause")) {
            *broker.state.snapshot_failure.lock().await = failure;
        } else {
            *broker.state.snapshot_qualification_failure.lock().await = failure;
        }
        let engine =
            crate::container_broker::engine::Engine::new(&broker.config.engine_socket, "/_ping")
                .await
                .unwrap();
        let result = engine.qualify(&broker.config).await;
        if matches!(failure, Some("identity" | "pause" | "export" | "unpause")) {
            assert!(result.is_err());
        } else {
            let contract = result.unwrap();
            assert!(contract.qualified);
            assert_eq!(contract.snapshots, failure.is_none());
        }
        assert!(broker.state.containers.lock().await.is_empty());
        let requests = broker.state.requests.lock().await;
        assert!(requests.iter().any(|request| request.ends_with("/pause")));
        assert_eq!(
            requests.iter().any(|request| request.ends_with("/unpause")),
            !matches!(failure, Some("pause" | "export"))
        );
    }
}

fn pivot_enospc_response() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "message":"Error setting up pivot dir: mkdir /engine/overlay/merged/.pivot_root12345: no space left on device"
    }))
    .unwrap()
}

async fn assert_resource_probe_cleaned(broker: &RestartBroker, exported: bool) {
    assert!(broker.state.containers.lock().await.is_empty());
    let requests = broker.state.requests.lock().await;
    let removed = requests
        .iter()
        .position(|request| request.starts_with("DELETE /containers/oc-qualification-"))
        .unwrap();
    let path = requests[removed].strip_prefix("DELETE ").unwrap();
    assert_eq!(
        requests[removed + 1..]
            .iter()
            .find(|request| request.starts_with("GET /containers/")),
        Some(&format!("GET {path}/json"))
    );
    assert_eq!(
        requests.iter().any(|request| request.ends_with("/export")),
        exported
    );
}

#[tokio::test]
async fn resource_probe_retries_only_transient_archive_pivot_enospc() {
    let broker = RestartBroker::new().await;
    *broker.state.marker_archive_error.lock().await = Some((500, pivot_enospc_response()));
    broker
        .state
        .marker_archive_error_once
        .store(true, Ordering::SeqCst);
    let engine =
        crate::container_broker::engine::Engine::new(&broker.config.engine_socket, "/_ping")
            .await
            .unwrap();
    let contract = engine.qualify(&broker.config).await.unwrap();
    assert!(contract.qualified && contract.snapshots);
    assert_eq!(broker.state.marker_archive_calls.load(Ordering::SeqCst), 2);
    assert_resource_probe_cleaned(&broker, true).await;
}

#[tokio::test]
async fn resource_probe_persistent_archive_pivot_enospc_keeps_original_deadline() {
    let broker = RestartBroker::new().await;
    *broker.state.marker_archive_error.lock().await = Some((500, pivot_enospc_response()));
    let engine =
        crate::container_broker::engine::Engine::new(&broker.config.engine_socket, "/_ping")
            .await
            .unwrap();
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        engine.qualify(&broker.config),
    )
    .await
    .unwrap();
    assert!(result.is_err());
    assert!(started.elapsed() >= std::time::Duration::from_secs(10));
    assert!(broker.state.marker_archive_calls.load(Ordering::SeqCst) > 1);
    assert_resource_probe_cleaned(&broker, false).await;
}

#[tokio::test]
async fn resource_probe_archive_errors_reject_other_status_and_invalid_classification() {
    let message = "Error setting up pivot dir: mkdir /engine/overlay/merged/.pivot_root12345: no space left on device";
    let mut errors = vec![
        (503, pivot_enospc_response()),
        (500, b"not-json".to_vec()),
        (500, vec![b'x'; 4097]),
        (
            500,
            serde_json::to_vec(&json!({"message":message,"extra":true})).unwrap(),
        ),
        (
            500,
            format!(
                "{{\"message\":{0},\"message\":{0}}}",
                serde_json::to_string(message).unwrap()
            )
            .into_bytes(),
        ),
    ];
    for message in [
        "unrelated Engine failure",
        "Error setting up pivot dir: mkdir /engine/merged/.pivot_root12345: permission denied",
        "Error setting up pivot dir: mkdir /engine/merged/ordinary-dir: no space left on device",
        "Error setting up pivot dir: mkdir relative/.pivot_root12345: no space left on device",
        "Error setting up pivot dir: mkdir /engine/../merged/.pivot_root12345: no space left on device",
        "Error setting up pivot dir: mkdir /engine/merged/.pivot_root: no space left on device",
        "Error setting up pivot dir: mkdir /engine/merged/.pivot_rootabc: no space left on device",
        "Error setting up pivot dir: mkdir /engine/\nmerged/.pivot_root12345: no space left on device",
    ] {
        errors.push((
            500,
            serde_json::to_vec(&json!({"message":message})).unwrap(),
        ));
    }
    for (status, body) in errors {
        let broker = RestartBroker::new().await;
        *broker.state.marker_archive_error.lock().await = Some((status, body));
        let engine =
            crate::container_broker::engine::Engine::new(&broker.config.engine_socket, "/_ping")
                .await
                .unwrap();
        assert!(engine.qualify(&broker.config).await.is_err());
        assert_eq!(broker.state.marker_archive_calls.load(Ordering::SeqCst), 1);
        assert_resource_probe_cleaned(&broker, false).await;
    }
}

#[tokio::test]
async fn resource_probe_pivot_enospc_cannot_hide_an_exited_container() {
    let broker = RestartBroker::new().await;
    *broker.state.marker_archive_error.lock().await = Some((500, pivot_enospc_response()));
    broker
        .state
        .marker_archive_exited
        .store(true, Ordering::SeqCst);
    let engine =
        crate::container_broker::engine::Engine::new(&broker.config.engine_socket, "/_ping")
            .await
            .unwrap();
    let contract = engine.qualify(&broker.config).await.unwrap();
    assert!(!contract.qualified && !contract.snapshots);
    assert_eq!(broker.state.marker_archive_calls.load(Ordering::SeqCst), 1);
    assert_resource_probe_cleaned(&broker, false).await;
}

#[tokio::test]
async fn snapshot_capture_requires_positive_snapshot_qualification() {
    let mut broker = RestartBroker::new().await;
    *broker.state.snapshot_qualification_failure.lock().await = Some("volatile");
    let provider = broker.start().await;
    assert!(!provider.supports_snapshots());
    let writes = broker
        .state
        .requests
        .lock()
        .await
        .iter()
        .filter(|request| request.ends_with("/pause"))
        .count();
    let lease = lease();
    assert!(
        provider
            .capture_snapshot(
                &lease.lease_key,
                &"1".repeat(64),
                4096,
                &broker.config.spool_dir
            )
            .await
            .is_err()
    );
    let response = broker
        .client()
        .post(format!(
            "http://localhost/open-compute/v1/leases/{}/snapshot",
            lease.lease_key
        ))
        .body(serde_json::to_vec(&json!({"startIdentity":"1".repeat(64),"maxBytes":4096})).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(
        broker
            .state
            .requests
            .lock()
            .await
            .iter()
            .filter(|request| request.ends_with("/pause"))
            .count(),
        writes
    );
    assert!(broker.state.containers.lock().await.is_empty());
    broker.retire().await;
}

fn archive(entries: usize) -> Vec<u8> {
    let mut archive = tar::Builder::new(Vec::new());
    for index in 0..entries {
        let mut header = tar::Header::new_ustar();
        header.set_mode(0o640);
        header.set_uid(1000);
        header.set_gid(1000);
        header.set_size(4);
        header.set_cksum();
        archive
            .append_data(
                &mut header,
                format!("data/file-{index}"),
                b"data".as_slice(),
            )
            .unwrap();
    }
    archive.into_inner().unwrap()
}

async fn running() -> (
    RestartBroker,
    QualifiedContainerProvider,
    BrokerLease,
    String,
) {
    let mut broker = RestartBroker::new().await;
    let provider = broker.start().await;
    let mut lease = lease();
    lease.provider_contract_sha256 = provider.contract_sha256().to_owned();
    lease.max_instances = broker.config.max_instances;
    provider.register(&lease).await.unwrap();
    provider
        .lease_credentials(&lease.lease_key, &"1".repeat(64))
        .await
        .unwrap();
    let client = broker.client();
    for proxy in [true, false] {
        let name = format!("{}{}", lease.lease_key, if proxy { "-proxy" } else { "" });
        assert_eq!(
            client
                .post(format!("http://localhost/containers/create?name={name}"))
                .body(serde_json::to_vec(&create(proxy)).unwrap())
                .send()
                .await
                .unwrap()
                .status(),
            201
        );
        assert_eq!(
            client
                .post(format!("http://localhost/containers/{name}/start"))
                .send()
                .await
                .unwrap()
                .status(),
            204
        );
    }
    let observed = provider.observe(&lease.lease_key).await.unwrap();
    assert!(observed.running);
    *broker.state.snapshot_archive.lock().await = Some(archive(1));
    (broker, provider, lease, observed.start_identity)
}

#[tokio::test]
async fn snapshot_capture_streams_large_entry_set_and_preserves_the_live_process() {
    let (mut broker, provider, lease, identity) = running().await;
    let expected = archive(5_000);
    *broker.state.snapshot_archive.lock().await = Some(expected.clone());
    let (mut file, size) = provider
        .capture_snapshot(
            &lease.lease_key,
            &identity,
            8 << 20,
            &broker.config.spool_dir,
        )
        .await
        .unwrap();
    let mut actual = Vec::new();
    file.read_to_end(&mut actual).unwrap();
    assert_eq!(actual, expected);
    assert_eq!(size, expected.len() as u64);
    assert!(
        std::fs::read_dir(&broker.config.state_dir)
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("pending-"))
    );
    let observed = provider.observe(&lease.lease_key).await.unwrap();
    assert!(observed.running && !observed.late_operations_fenced);
    assert_eq!(observed.start_identity, identity);
    assert_eq!(
        broker.state.containers.lock().await[&lease.lease_key]["State"]["Paused"],
        false
    );
    let id = broker.state.containers.lock().await[&lease.lease_key]["Id"]
        .as_str()
        .unwrap()
        .to_owned();
    let requests = broker.state.requests.lock().await.clone();
    for stage in ["pause", "export", "unpause"] {
        assert!(
            requests
                .iter()
                .any(|request| request.ends_with(&format!("/containers/{id}/{stage}")))
        );
    }
    provider.terminate(&lease.lease_key).await.unwrap();
    broker.retire().await;
}

#[tokio::test]
async fn snapshot_capture_rejects_authority_and_limits_before_engine_mutation() {
    let (mut broker, provider, lease, identity) = running().await;
    let endpoint = format!(
        "http://localhost/open-compute/v1/leases/{}/snapshot",
        lease.lease_key
    );
    let before = broker.state.requests.lock().await.len();
    for (key, expected, maximum) in [
        ("f".repeat(64), identity.clone(), 4096),
        (lease.lease_key.clone(), "f".repeat(64), 4096),
        (lease.lease_key.clone(), identity.clone(), 0),
        (
            lease.lease_key.clone(),
            identity.clone(),
            provider.max_image_bytes() + 1,
        ),
    ] {
        assert!(
            provider
                .capture_snapshot(&key, &expected, maximum, &broker.config.spool_dir)
                .await
                .is_err()
        );
    }
    let response = broker
        .client()
        .post(&endpoint)
        .body(
            serde_json::to_vec(
                &json!({"startIdentity":identity,"maxBytes":4096,"container":"foreign"}),
            )
            .unwrap(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    for request in [
        json!({"startIdentity":identity,"maxBytes":0}),
        json!({"startIdentity":identity,"maxBytes":provider.max_image_bytes()+1}),
    ] {
        assert_eq!(
            broker
                .client()
                .post(&endpoint)
                .body(serde_json::to_vec(&request).unwrap())
                .send()
                .await
                .unwrap()
                .status(),
            503
        );
    }
    assert_eq!(
        broker
            .client()
            .get(format!(
                "http://localhost/containers/{}/export",
                lease.lease_key
            ))
            .send()
            .await
            .unwrap()
            .status(),
        503
    );
    assert!(
        broker.state.requests.lock().await[before..]
            .iter()
            .all(|request| !request.ends_with("/pause") && !request.ends_with("/export"))
    );
    assert!(
        !broker
            .config
            .state_dir
            .join(format!("pending-{}", lease.lease_key))
            .exists()
    );
    let label = broker.state.containers.lock().await[&lease.lease_key]["Config"]["Labels"]["dev.open-compute.container.owner"].clone();
    broker
        .state
        .containers
        .lock()
        .await
        .get_mut(&lease.lease_key)
        .unwrap()["Config"]["Labels"]["dev.open-compute.container.owner"] = json!("f".repeat(64));
    assert!(
        provider
            .capture_snapshot(&lease.lease_key, &identity, 4096, &broker.config.spool_dir)
            .await
            .is_err()
    );
    broker
        .state
        .containers
        .lock()
        .await
        .get_mut(&lease.lease_key)
        .unwrap()["Config"]["Labels"]["dev.open-compute.container.owner"] = label;
    let original_id = broker.state.containers.lock().await[&lease.lease_key]["Id"].clone();
    broker
        .state
        .containers
        .lock()
        .await
        .get_mut(&lease.lease_key)
        .unwrap()["Id"] = json!("f".repeat(64));
    assert!(
        provider
            .capture_snapshot(&lease.lease_key, &identity, 4096, &broker.config.spool_dir)
            .await
            .is_err()
    );
    broker
        .state
        .containers
        .lock()
        .await
        .get_mut(&lease.lease_key)
        .unwrap()["Id"] = original_id;
    provider.terminate(&lease.lease_key).await.unwrap();
    broker.retire().await;
}

#[tokio::test]
async fn snapshot_capture_failed_writes_and_invalid_tar_require_positive_cleanup() {
    for stage in [
        "pause",
        "export",
        "unpause",
        "tar",
        "bytes",
        "changed-start",
    ] {
        let (mut broker, provider, lease, identity) = running().await;
        let maximum = if stage == "bytes" { 512 } else { 4096 };
        match stage {
            "tar" => *broker.state.snapshot_archive.lock().await = Some(b"invalid-tar".to_vec()),
            "changed-start" => broker
                .state
                .snapshot_changed_start
                .store(true, Ordering::SeqCst),
            "bytes" => {}
            _ => *broker.state.snapshot_failure.lock().await = Some(stage),
        }
        assert!(
            provider
                .capture_snapshot(
                    &lease.lease_key,
                    &identity,
                    maximum,
                    &broker.config.spool_dir
                )
                .await
                .is_err(),
            "{stage}"
        );
        assert!(
            broker
                .config
                .state_dir
                .join(format!("pending-{}", lease.lease_key))
                .exists()
        );
        let writes = broker
            .state
            .requests
            .lock()
            .await
            .iter()
            .filter(|request| request.ends_with("/pause"))
            .count();
        assert!(
            provider
                .capture_snapshot(&lease.lease_key, &identity, 4096, &broker.config.spool_dir)
                .await
                .is_err()
        );
        assert!(
            provider
                .lease_credentials(&lease.lease_key, &"2".repeat(64))
                .await
                .is_err()
        );
        assert_eq!(
            broker
                .state
                .requests
                .lock()
                .await
                .iter()
                .filter(|request| request.ends_with("/pause"))
                .count(),
            writes
        );
        *broker.state.snapshot_failure.lock().await = None;
        provider.terminate(&lease.lease_key).await.unwrap();
        assert!(broker.state.containers.lock().await.is_empty());
        assert!(
            !broker
                .config
                .state_dir
                .join(format!("pending-{}", lease.lease_key))
                .exists()
        );
        broker.retire().await;
    }
}

#[tokio::test]
async fn snapshot_capture_cancellation_before_and_after_durable_begin_survives_restart() {
    for after_begin in [false, true] {
        let (mut broker, provider, lease, identity) = running().await;
        let gate = Arc::new(tokio::sync::Notify::new());
        if after_begin {
            *broker.state.snapshot_gate.lock().await = Some(("export", gate.clone()));
        } else {
            *broker.state.inspect_gate.lock().await = Some(gate.clone());
        }
        let capture_provider = provider.clone();
        let key = lease.lease_key.clone();
        let spool = broker.config.spool_dir.clone();
        let capture = tokio::spawn(async move {
            capture_provider
                .capture_snapshot(&key, &identity, 4096, &spool)
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            if after_begin {
                broker.state.snapshot_started.notified().await;
            } else {
                broker.state.inspect_started.notified().await;
            }
        })
        .await
        .unwrap();
        broker.retire().await;
        assert!(capture.await.unwrap().is_err());
        assert_eq!(
            broker
                .config
                .state_dir
                .join(format!("pending-{}", lease.lease_key))
                .exists(),
            after_begin
        );
        gate.notify_waiters();
        let provider = broker.start().await;
        if after_begin {
            assert!(provider.register(&lease).await.is_ok());
            assert!(
                provider
                    .lease_credentials(&lease.lease_key, &"2".repeat(64))
                    .await
                    .is_err()
            );
        } else {
            provider.adopt_for_cleanup(&lease).await.unwrap();
        }
        provider.terminate(&lease.lease_key).await.unwrap();
        assert!(broker.state.containers.lock().await.is_empty());
        broker.retire().await;
    }
}
