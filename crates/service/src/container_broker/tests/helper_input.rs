//! Immutable helper supply, mount policy and read-only Engine transport regressions.

use super::*;
use crate::container_broker::helper;

const CONTENT: &[u8] = b"trusted helper";

pub(crate) fn mounted_volume() -> Value {
    json!([{"Type":"volume","Name":config().exec_helper_volume,"Driver":"local",
        "Source":"/engine-owned-volume","Destination":"/.open-compute-exec",
        "RW":false,"Propagation":""}])
}

pub(in crate::container_broker::tests) fn mounts_from_request(host: &Value) -> Value {
    match host.get("Mounts").and_then(Value::as_array) {
        None => json!([]),
        Some(mounts) => {
            json!(
            mounts
                .iter()
                .map(|mount| if mount["Type"] == "bind" { json!({
                    "Type":"bind","Source":mount["Source"],"Destination":mount["Target"],
                    "RW":!mount["ReadOnly"].as_bool().unwrap_or(false),"Propagation":mount.pointer("/BindOptions/Propagation").and_then(Value::as_str).unwrap_or("rprivate")
                }) } else { json!({
                    "Type":mount["Type"],"Name":mount["Source"],"Driver":"local",
                    "Source":"/engine-owned-volume","Destination":mount["Target"],
                    "RW":!mount["ReadOnly"].as_bool().unwrap_or(false),"Propagation":""
                }) })
                .collect::<Vec<_>>()
        )
        }
    }
}

fn volume() -> Value {
    json!({"Name":config().exec_helper_volume,"Driver":"local","Scope":"local","Options":{},
        "Labels":{"dev.open-compute.exec-helper.image":contract().sidecar_image,
        "dev.open-compute.exec-helper.sha256":contract().exec_helper_sha256}})
}

fn archive(content: &[u8], mode: u32, kind: tar::EntryType) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_ustar();
    header.set_mode(mode);
    header.set_entry_type(kind);
    header.set_size(if kind.is_file() {
        content.len() as u64
    } else {
        0
    });
    if kind.is_symlink() || kind.is_hard_link() {
        header.set_link_name("other").unwrap();
    }
    header.set_cksum();
    builder
        .append_data(
            &mut header,
            "helper",
            if kind.is_file() { content } else { &[] },
        )
        .unwrap();
    builder.into_inner().unwrap()
}

pub(in crate::container_broker::tests::transport) async fn handle(
    state: &MockEngine,
    method: &Method,
    path: &str,
    query: &BTreeMap<String, String>,
) -> Option<Response<Body>> {
    if path.starts_with("/volumes/") {
        assert_eq!(*method, Method::GET, "provider input is never mutated");
        assert_eq!(path, format!("/volumes/{}", config().exec_helper_volume));
        let (status, value) = state
            .helper_volume_override
            .lock()
            .await
            .clone()
            .unwrap_or((200, volume()));
        return Some(
            Response::builder()
                .status(status)
                .body(Body::from(serde_json::to_vec(&value).unwrap()))
                .unwrap(),
        );
    }
    if *method != Method::GET
        || !path.ends_with("/archive")
        || query.get("path").map(String::as_str) != Some(helper::PATH)
    {
        return None;
    }
    let name = path
        .strip_prefix("/containers/")?
        .strip_suffix("/archive")?;
    let containers = state.containers.lock().await;
    let Some(container) = containers
        .iter()
        .find_map(|(key, value)| (key == name || value["Id"] == name).then_some(value))
    else {
        return Some(Response::builder().status(404).body(Body::empty()).unwrap());
    };
    let mounted = container["HostConfig"].get("Mounts").is_some();
    let override_value = if mounted {
        state.helper_archive_override.lock().await.clone()
    } else {
        state.helper_image_archive_override.lock().await.clone()
    };
    let (status, bytes) =
        override_value.unwrap_or((200, archive(CONTENT, 0o755, tar::EntryType::Regular)));
    Some(
        Response::builder()
            .status(status)
            .body(Body::from(bytes))
            .unwrap(),
    )
}

#[test]
fn helper_input_rejects_foreign_volume_and_noncanonical_mounts() {
    helper::verify_volume(&volume(), &config()).unwrap();
    for (pointer, forged) in [
        ("/Name", json!("foreign")),
        ("/Driver", json!("nfs")),
        ("/Scope", json!("global")),
        ("/Options", json!({"device":"/host"})),
        (
            "/Labels/dev.open-compute.exec-helper.image",
            json!(lease().image),
        ),
        (
            "/Labels/dev.open-compute.exec-helper.sha256",
            json!("0".repeat(64)),
        ),
    ] {
        let mut bad = volume();
        *bad.pointer_mut(pointer).unwrap() = forged;
        assert!(helper::verify_volume(&bad, &config()).is_err(), "{pointer}");
    }
    let mut extra = volume();
    extra["Labels"]["foreign"] = json!("value");
    assert!(helper::verify_volume(&extra, &config()).is_err());
    for name in [
        "",
        "../other",
        "/host/path",
        "a?query",
        "a%2Fb",
        "-leading",
        &"a".repeat(129),
    ] {
        let mut bad = config();
        bad.exec_helper_volume = name.into();
        assert!(bad.validate().is_err());
    }
    let valid = json!({"HostConfig":{"Mounts":helper::mount(&config().exec_helper_volume)}, "Mounts":mounted_volume()});
    helper::verify_mounts(&valid, &config().exec_helper_volume).unwrap();
    for (pointer, forged) in [
        ("/HostConfig/Mounts/0/ReadOnly", json!(false)),
        ("/HostConfig/Mounts/0/VolumeOptions/NoCopy", json!(false)),
        ("/Mounts/0/Name", json!("foreign")),
        ("/Mounts/0/Type", json!("bind")),
        ("/Mounts/0/RW", json!(true)),
        ("/Mounts/0/Destination", json!("/run")),
        ("/Mounts/0/Driver", json!("nfs")),
        ("/Mounts/0/Propagation", json!("rshared")),
    ] {
        let mut bad = valid.clone();
        *bad.pointer_mut(pointer).unwrap() = forged;
        assert!(
            helper::verify_mounts(&bad, &config().exec_helper_volume).is_err(),
            "{pointer}"
        );
    }
    let mut bad = valid;
    bad["Mounts"]
        .as_array_mut()
        .unwrap()
        .push(json!({"Type":"bind"}));
    assert!(helper::verify_mounts(&bad, &config().exec_helper_volume).is_err());
}

#[test]
fn helper_input_requires_exact_regular_executable_bytes() {
    let digest = config().exec_helper_sha256;
    helper::verify_archive(&archive(CONTENT, 0o755, tar::EntryType::Regular), &digest).unwrap();
    for (bytes, mode, kind) in [
        (CONTENT, 0o644, tar::EntryType::Regular),
        (CONTENT, 0o4755, tar::EntryType::Regular),
        (CONTENT, 0o755, tar::EntryType::Symlink),
        (CONTENT, 0o755, tar::EntryType::Link),
        (b"tampered".as_slice(), 0o755, tar::EntryType::Regular),
        (b"".as_slice(), 0o755, tar::EntryType::Regular),
    ] {
        assert!(helper::verify_archive(&archive(bytes, mode, kind), &digest).is_err());
    }
    let mut truncated = archive(CONTENT, 0o755, tar::EntryType::Regular);
    truncated.truncate(515);
    assert!(helper::verify_archive(&truncated, &digest).is_err());
    let mut extra = archive(CONTENT, 0o755, tar::EntryType::Regular);
    extra.truncate(1024);
    extra.extend(archive(CONTENT, 0o755, tar::EntryType::Regular));
    assert!(helper::verify_archive(&extra, &digest).is_err());
    let mut large = archive(CONTENT, 0o755, tar::EntryType::Regular);
    let mut header = tar::Header::from_byte_slice(&large[..512]).clone();
    header.set_size(helper::MAX_BYTES + 1);
    header.set_cksum();
    large[..512].copy_from_slice(header.as_bytes());
    assert!(helper::verify_archive(&large, &digest).is_err());
}

#[tokio::test]
async fn helper_input_qualification_proves_image_and_volume_bytes_without_mutating_input() {
    for failure in [
        "none",
        "missing",
        "labels",
        "volume-bytes",
        "image-bytes",
        "mode",
        "symlink",
        "mount-export",
    ] {
        let broker = RestartBroker::new().await;
        match failure {
            "missing" => {
                *broker.state.helper_volume_override.lock().await = Some((404, Value::Null));
            }
            "labels" => {
                let mut bad = volume();
                bad["Labels"] = json!({});
                *broker.state.helper_volume_override.lock().await = Some((200, bad));
            }
            "volume-bytes" => {
                *broker.state.helper_archive_override.lock().await =
                    Some((200, archive(b"wrong", 0o755, tar::EntryType::Regular)));
            }
            "image-bytes" => {
                *broker.state.helper_image_archive_override.lock().await =
                    Some((200, archive(b"wrong", 0o755, tar::EntryType::Regular)));
            }
            "mode" => {
                *broker.state.helper_archive_override.lock().await =
                    Some((200, archive(CONTENT, 0o644, tar::EntryType::Regular)));
            }
            "symlink" => {
                *broker.state.helper_archive_override.lock().await =
                    Some((200, archive(CONTENT, 0o755, tar::EntryType::Symlink)));
            }
            "mount-export" => {
                *broker.state.snapshot_qualification_failure.lock().await = Some("helper");
            }
            _ => {}
        }
        let engine =
            crate::container_broker::engine::Engine::new(&broker.config.engine_socket, "/_ping")
                .await
                .unwrap();
        let result = engine.qualify(&broker.config).await;
        match failure {
            "none" => {
                let contract = result.unwrap();
                assert!(contract.qualified && contract.snapshots);
                assert_eq!(
                    contract.exec_helper_sha256,
                    broker.config.exec_helper_sha256
                );
            }
            "mount-export" => assert!(!result.unwrap().snapshots),
            _ => assert!(result.is_err(), "{failure}"),
        }
        assert!(broker.state.containers.lock().await.is_empty());
        let requests = broker.state.requests.lock().await;
        assert!(
            requests
                .iter()
                .any(|request| request.starts_with("GET /volumes/"))
        );
        assert!(
            !requests
                .iter()
                .any(|request| request.starts_with("POST /volumes")
                    || request.starts_with("DELETE /volumes"))
        );
        if matches!(failure, "missing" | "labels") {
            assert!(
                !requests
                    .iter()
                    .any(|request| request == "POST /containers/create")
            );
        }
    }
}

#[tokio::test]
async fn helper_input_change_blocks_create_or_start_but_preserves_positive_cleanup() {
    let mut broker = RestartBroker::new().await;
    let provider = broker.start().await;
    let mut lease = lease();
    lease.provider_contract_sha256 = provider.contract_sha256().into();
    lease.max_instances = broker.config.max_instances;
    provider.register(&lease).await.unwrap();
    provider
        .lease_credentials(&lease.lease_key, &"1".repeat(64))
        .await
        .unwrap();
    let client = broker.client();
    let proxy_name = format!("{}-proxy", lease.lease_key);
    assert_eq!(
        client
            .post(format!(
                "http://localhost/containers/create?name={proxy_name}"
            ))
            .body(serde_json::to_vec(&create(true)).unwrap())
            .send()
            .await
            .unwrap()
            .status(),
        201
    );
    assert_eq!(
        client
            .post(format!("http://localhost/containers/{proxy_name}/start"))
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    let endpoint = format!(
        "http://localhost/containers/create?name={}",
        lease.lease_key
    );
    *broker.state.helper_volume_override.lock().await = Some((404, Value::Null));
    let before = broker
        .state
        .requests
        .lock()
        .await
        .iter()
        .filter(|request| *request == "POST /containers/create")
        .count();
    assert_eq!(
        client
            .post(&endpoint)
            .body(serde_json::to_vec(&create(false)).unwrap())
            .send()
            .await
            .unwrap()
            .status(),
        503
    );
    assert_eq!(
        broker
            .state
            .requests
            .lock()
            .await
            .iter()
            .filter(|request| *request == "POST /containers/create")
            .count(),
        before
    );
    *broker.state.helper_volume_override.lock().await = None;
    *broker.state.helper_archive_override.lock().await = Some((404, Vec::new()));
    assert_eq!(
        client
            .post(&endpoint)
            .body(serde_json::to_vec(&create(false)).unwrap())
            .send()
            .await
            .unwrap()
            .status(),
        503
    );
    assert!(
        !broker.state.containers.lock().await[&lease.lease_key]["State"]["Running"]
            .as_bool()
            .unwrap()
    );
    let start = format!("http://localhost/containers/{}/start", lease.lease_key);
    assert_eq!(client.post(&start).send().await.unwrap().status(), 503);
    *broker.state.helper_archive_override.lock().await = None;
    assert_eq!(client.post(&start).send().await.unwrap().status(), 204);
    let engine = reqwest::Client::builder()
        .unix_socket(broker.config.engine_socket.clone())
        .no_proxy()
        .build()
        .unwrap();
    let volume_endpoint = format!("http://localhost/volumes/{}", config().exec_helper_volume);
    let original_input = engine
        .get(&volume_endpoint)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    *broker.state.helper_volume_override.lock().await = Some((404, Value::Null));
    *broker.state.helper_archive_override.lock().await = Some((404, Vec::new()));
    provider.terminate(&lease.lease_key).await.unwrap();
    assert!(broker.state.containers.lock().await.is_empty());
    *broker.state.helper_volume_override.lock().await = None;
    assert_eq!(
        engine
            .get(&volume_endpoint)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
        original_input
    );
    broker.retire().await;
    *broker.state.helper_volume_override.lock().await = None;
    *broker.state.helper_archive_override.lock().await = None;
    let provider = broker.start().await;
    assert!(provider.ensure_active().await.is_ok());
    broker.retire().await;
}
