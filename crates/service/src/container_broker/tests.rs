use super::{contract::*, policy::*};
use axum::http::{Method, Uri};
use open_compute_core::containers::ContainerResources;
use serde_json::{Value, json};
use sha2::Digest as _;
use std::{os::unix::fs::PermissionsExt, path::PathBuf};
#[path = "tests/exec.rs"]
mod exec;
#[path = "tests/image_size.rs"]
mod image_size;
#[path = "tests/streams.rs"]
mod streams;
#[path = "tests/transport.rs"]
mod transport;

fn resources() -> ContainerResources {
    ContainerResources {
        cpu_nanos: 62_500_000,
        memory_bytes: 67_108_864,
        disk_bytes: 16_777_216,
        pids: 16,
        open_files: 64,
    }
}

fn contract() -> ProviderContract {
    ProviderContract {
        version: ProviderContract::VERSION,
        runtime: "docker".into(),
        engine_version: "29.4.0".into(),
        architecture: "amd64".into(),
        isolation: "linux-private-pid-seccomp-cgroup-v2-overlay2".into(),
        qualified: true,
        snapshots: false,
        sidecar_image: Some(format!("sha256:{}", "b".repeat(64))),
        exec_helper_sha256: hex::encode(sha2::Sha256::digest(b"trusted helper")),
        resources: resources(),
        max_instances: 4,
        max_image_bytes: 512 << 20,
        max_stream_bytes: 64 << 20,
    }
}

fn lease() -> BrokerLease {
    BrokerLease {
        directory_snapshots: Vec::new(),
        scheduling_policy: open_compute_storage::containers::ContainerSchedulingPolicy::Default,
        lease_key: "a".repeat(64),
        owner_key: "9".repeat(64),
        startup: crate::container_broker::BrokerStartOptions {
            enable_internet: false,
            entrypoint: Some(vec!["/bin/sh".into()]),
            environment_variables: vec!["KEY=value".into()],
            labels: Vec::new(),
        },
        start_identity: "c".repeat(64),
        manifest_sha256: "d".repeat(64),
        image: format!("sha256:{}", "e".repeat(64)),
        diff_ids: vec![format!("sha256:{}", "f".repeat(64))],
        provider_contract_sha256: contract().sha256().unwrap(),
        resources: resources(),
        application_id: open_compute_core::ResourceId::generate(),
        max_instances: 2,
    }
}

fn config() -> BrokerConfig {
    BrokerConfig {
        socket: PathBuf::from("/tmp/private/broker.sock"),
        engine_socket: PathBuf::from("/tmp/docker.sock"),
        peer_uid: rustix::process::getuid().as_raw(),
        sidecar_image: contract().sidecar_image,
        exec_helper_volume: "open-compute-exec-test".into(),
        exec_helper_sha256: contract().exec_helper_sha256,
        qualification_image: lease().image,
        max_leases: 100,
        max_instances: 4,
        max_requests: 8,
        relay_host: "127.0.0.1".parse().unwrap(),
        spool_dir: PathBuf::from("/tmp/private"),
        state_dir: PathBuf::from("/tmp/private-state"),
        max_image_bytes: 512 << 20,
        max_stream_bytes: 64 << 20,
        resources: resources(),
    }
}

fn create(sidecar: bool) -> Value {
    if sidecar {
        json!({"Image":contract().sidecar_image.as_deref().unwrap(),"Cmd":["--http-egress-port","40000","--http-ingress-address","0.0.0.0:39001","--docker-gateway-cidr","172.17.0.0/16","--dns-enabled","--tls-intercept","--control-auth-file","/run/open-compute/control.json","--disable-ipv6"],
        "ExposedPorts":{"39001/tcp":{}},"HostConfig":{"NetworkMode":"bridge","PublishAllPorts":true,"Dns":["1.1.1.1","8.8.8.8"],"ExtraHosts":["host.docker.internal:host-gateway"],"CapAdd":["NET_ADMIN"]}})
    } else {
        json!({"Image":lease().image,"Env":["KEY=value"],"Entrypoint":["/bin/sh"],"Cmd":[],"HostConfig":{"NetworkMode":format!("container:{}-proxy",lease().lease_key),"RestartPolicy":{"Name":"no"},"CapDrop":["NET_RAW"]}})
    }
}

#[test]
fn container_broker_contract_rejects_unqualified_or_mutable_policy() {
    config().validate().unwrap();
    lease().validate(&contract()).unwrap();
    assert_eq!(contract().sha256().unwrap().len(), 64);
    let mut old_contract = contract();
    old_contract.version = 2;
    assert!(lease().validate(&old_contract).is_err());
    let mut missing_policy = serde_json::to_value(lease()).unwrap();
    missing_policy
        .as_object_mut()
        .unwrap()
        .remove("schedulingPolicy");
    assert!(serde_json::from_value::<BrokerLease>(missing_policy).is_err());
    for policy in [json!("regional"), json!(null), json!({"privileged":true})] {
        let mut invalid_policy = serde_json::to_value(lease()).unwrap();
        invalid_policy["schedulingPolicy"] = policy;
        assert!(serde_json::from_value::<BrokerLease>(invalid_policy).is_err());
    }
    for field in [
        "leaseKey",
        "startIdentity",
        "manifestSha256",
        "image",
        "providerContractSha256",
    ] {
        let mut value = serde_json::to_value(lease()).unwrap();
        value[field] = "bad".into();
        let lease: BrokerLease = serde_json::from_value(value).unwrap();
        assert!(lease.validate(&contract()).is_err());
    }
    let mut unqualified = contract();
    unqualified.qualified = false;
    assert!(lease().validate(&unqualified).is_err());
    let mut value = lease();
    value.max_instances = 0;
    assert!(value.validate(&contract()).is_err());
    let mut value = lease();
    value.diff_ids = vec!["latest".into()];
    assert!(value.validate(&contract()).is_err());
    let mut value = lease();
    value.resources.memory_bytes += 1;
    assert!(value.validate(&contract()).is_err());
    for field in [
        "socket",
        "engine_socket",
        "sidecar_image",
        "qualification_image",
        "state_dir",
    ] {
        let mut value = serde_json::to_value(json!({"socket":"/tmp/private/broker.sock","engine_socket":"/tmp/docker.sock","peer_uid":0,"sidecar_image":contract().sidecar_image.as_deref().unwrap(),"qualification_image":lease().image,"exec_helper_volume":config().exec_helper_volume,"exec_helper_sha256":config().exec_helper_sha256,"max_leases":100,"max_instances":4,"max_requests":8,"relay_host":"127.0.0.1","resources":resources(),"spool_dir":"/tmp/private","state_dir":"/tmp/private-state","max_image_bytes":536870912,"max_stream_bytes":67108864})).unwrap();
        value[field] = "relative".into();
        assert!(
            serde_json::from_value::<BrokerConfig>(value)
                .unwrap()
                .validate()
                .is_err()
        );
    }
    let mut config = config();
    config.engine_socket = config.socket.clone();
    assert!(config.validate().is_err());
}

#[test]
fn container_broker_routes_are_closed_and_queries_are_exact() {
    let key = lease().lease_key;
    let cases = [
        (Method::GET, "/networks/bridge".to_owned()),
        (Method::POST, format!("/containers/create?name={key}")),
        (Method::GET, format!("/containers/{key}/json")),
        (Method::POST, format!("/containers/{key}/start")),
        (Method::POST, format!("/containers/{key}/stop")),
        (Method::POST, format!("/containers/{key}/kill?signal=31")),
        (Method::POST, format!("/containers/{key}/wait")),
        (
            Method::POST,
            format!("/containers/{key}/wait?condition=removed"),
        ),
        (Method::DELETE, format!("/containers/{key}?force=true")),
        (
            Method::PUT,
            format!("/containers/{key}/archive?path=%2Fetc"),
        ),
        (Method::POST, format!("/containers/{key}/exec")),
        (Method::POST, format!("/exec/{key}/start")),
        (Method::POST, format!("/exec/{key}/resize?h=0&w=80")),
        (Method::GET, format!("/exec/{key}/json")),
    ];
    for (method, path) in cases {
        let value = operation(&method, &path.parse::<Uri>().unwrap()).unwrap();
        assert_eq!(value.identity().is_none(), path == "/networks/bridge");
    }
    for path in [
        "/containers/json".into(),
        "/images/create?fromImage=latest".into(),
        format!("/containers/{key}/json?unknown=1"),
        format!("/containers/create?name={key}&name={key}"),
        format!("/containers/create?name={key}&privileged=true"),
        format!("/containers/{key}/kill?signal=bad"),
        format!("/containers/{key}/wait?condition=next-exit"),
        format!("/containers/{key}/archive?path=/../etc"),
        format!("/exec/{key}/resize?h=65536&w=80"),
        format!("/containers/{key}//json"),
        format!("/containers/%61{}/json", &key[1..]),
    ] {
        for method in [Method::GET, Method::POST, Method::PUT, Method::DELETE] {
            assert!(
                operation(&method, &path.parse::<Uri>().unwrap()).is_err(),
                "{path}"
            );
        }
    }
}

#[test]
fn container_broker_create_injects_limits_and_rejects_privilege() {
    let lease = lease();
    for sidecar in [false, true] {
        let name = format!("{}{}", lease.lease_key, if sidecar { "-proxy" } else { "" });
        let body = prepare_create(
            &serde_json::to_vec(&create(sidecar)).unwrap(),
            &name,
            &lease,
            contract().sidecar_image.as_deref().unwrap(),
            &lease.startup.environment_variables,
            &config().exec_helper_volume,
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["HostConfig"]["Memory"], lease.resources.memory_bytes);
        assert_eq!(value["HostConfig"]["NanoCpus"], 62_500_000);
        if !sidecar {
            assert_eq!(
                value["HostConfig"]["Tmpfs"],
                json!({"/run":format!("rw,exec,nosuid,nodev,size={}",lease.resources.memory_bytes)})
            );
        } else {
            assert!(value["HostConfig"].get("Tmpfs").is_none());
        }
        assert_eq!(
            value["Labels"]["dev.open-compute.container.lease"],
            lease.lease_key
        );
        for field in [
            "Privileged",
            "Binds",
            "Devices",
            "PidMode",
            "Mounts",
            "SecurityOpt",
            "CapAdd",
            "CapDrop",
            "Tmpfs",
        ] {
            let mut bad = create(sidecar);
            bad["HostConfig"][field] = json!(true);
            assert!(
                prepare_create(
                    &serde_json::to_vec(&bad).unwrap(),
                    &name,
                    &lease,
                    contract().sidecar_image.as_deref().unwrap(),
                    &lease.startup.environment_variables,
                    &config().exec_helper_volume,
                )
                .is_err()
            );
        }
        let mut bad = create(sidecar);
        bad["Image"] = "latest".into();
        assert!(
            prepare_create(
                &serde_json::to_vec(&bad).unwrap(),
                &name,
                &lease,
                contract().sidecar_image.as_deref().unwrap(),
                &lease.startup.environment_variables,
                &config().exec_helper_volume,
            )
            .is_err()
        );
        let mut bad = create(sidecar);
        bad["Untrusted"] = true.into();
        assert!(
            prepare_create(
                &serde_json::to_vec(&bad).unwrap(),
                &name,
                &lease,
                contract().sidecar_image.as_deref().unwrap(),
                &lease.startup.environment_variables,
                &config().exec_helper_volume,
            )
            .is_err()
        );
    }
    for pointer in [
        "/Cmd/0",
        "/Cmd/1",
        "/Cmd/2",
        "/Cmd/3",
        "/Cmd/4",
        "/Cmd/5",
        "/Cmd/6",
        "/Cmd/7",
        "/Cmd/8",
        "/HostConfig/NetworkMode",
        "/HostConfig/Dns/0",
        "/HostConfig/ExtraHosts/0",
    ] {
        let mut bad = create(true);
        *bad.pointer_mut(pointer).unwrap() = "untrusted".into();
        assert!(
            prepare_create(
                &serde_json::to_vec(&bad).unwrap(),
                &format!("{}-proxy", lease.lease_key),
                &lease,
                contract().sidecar_image.as_deref().unwrap(),
                &lease.startup.environment_variables,
                &config().exec_helper_volume,
            )
            .is_err()
        );
    }
    let mut bad = create(false);
    bad["Env"] = json!(["NUL\0"]);
    assert!(
        prepare_create(
            &serde_json::to_vec(&bad).unwrap(),
            &lease.lease_key,
            &lease,
            contract().sidecar_image.as_deref().unwrap(),
            &lease.startup.environment_variables,
            &config().exec_helper_volume,
        )
        .is_err()
    );
}

#[test]
fn container_broker_exec_and_image_identity_are_strict() {
    for value in [
        json!({"Cmd":["echo","hi"],"AttachStdout":true}),
        json!({"Cmd":["sh"],"Env":["A=B"],"WorkingDir":"/","User":"0","Tty":false}),
    ] {
        validate_exec(&serde_json::to_vec(&value).unwrap(), false).unwrap();
    }
    for value in [
        json!({"Detach":false,"Tty":true,"ConsoleSize":[0,80]}),
        json!({"Detach":true,"Tty":false}),
    ] {
        validate_exec(&serde_json::to_vec(&value).unwrap(), true).unwrap();
    }
    for (value, start) in [
        (json!({"Cmd":[]}), false),
        (json!({"Cmd":["bad\0"]}), false),
        (json!({"Cmd":["echo"],"Privileged":true}), false),
        (json!({"Cmd":["echo"],"User":true}), false),
        (json!({"Detach":"yes"}), true),
        (json!({"ConsoleSize":[0,0]}), true),
        (json!({"ConsoleSize":[1,65536]}), true),
        (json!({"Tty":"yes"}), true),
    ] {
        assert!(validate_exec(&serde_json::to_vec(&value).unwrap(), start).is_err());
    }
    let lease = lease();
    let image = json!({"Id":lease.image,"Os":"linux","Architecture":"amd64","RootFS":{"Type":"layers","Layers":lease.diff_ids},"Config":{"Volumes":{}}});
    assert!(image_matches(
        &image,
        &lease.image,
        &lease.diff_ids,
        "amd64"
    ));
    for pointer in ["/Id", "/RootFS/Type", "/RootFS/Layers/0"] {
        let mut bad = image.clone();
        *bad.pointer_mut(pointer).unwrap() = "bad".into();
        assert!(!image_matches(&bad, &lease.image, &lease.diff_ids, "amd64"));
    }
    let mut bad = image;
    bad["Config"]["Volumes"] = json!({"/host":{}});
    assert!(!image_matches(&bad, &lease.image, &lease.diff_ids, "amd64"));
}

#[test]
fn container_broker_inspection_checks_effective_spec_and_identity() {
    let lease = lease();
    let sidecar_id = "1".repeat(64);
    for sidecar in [true, false] {
        let name = format!("{}{}", lease.lease_key, if sidecar { "-proxy" } else { "" });
        let body = prepare_create(
            &serde_json::to_vec(&create(sidecar)).unwrap(),
            &name,
            &lease,
            contract().sidecar_image.as_deref().unwrap(),
            &lease.startup.environment_variables,
            &config().exec_helper_volume,
        )
        .unwrap();
        let mut body: Value = serde_json::from_slice(&body).unwrap();
        let expected_id = if sidecar {
            None
        } else {
            Some(sidecar_id.as_str())
        };
        if !sidecar {
            body["HostConfig"]["NetworkMode"] = json!(format!("container:{sidecar_id}"));
        }
        let value = json!({"Image":body["Image"],"Config":{"Labels":body["Labels"]},"HostConfig":body["HostConfig"], "Mounts":if sidecar {json!([])} else {transport::helper_input::mounted_volume()}});
        verify_inspect(
            &value,
            &name,
            &lease,
            contract().sidecar_image.as_deref().unwrap(),
            expected_id,
            &config().exec_helper_volume,
        )
        .unwrap();
        for tmpfs in [json!({}), json!({"/run":"size=1"}), json!({"/other":"rw"})] {
            let mut bad = value.clone();
            bad["HostConfig"]["Tmpfs"] = tmpfs;
            if !sidecar || bad["HostConfig"]["Tmpfs"] != json!({}) {
                assert!(
                    verify_inspect(
                        &bad,
                        &name,
                        &lease,
                        contract().sidecar_image.as_deref().unwrap(),
                        expected_id,
                        &config().exec_helper_volume
                    )
                    .is_err()
                );
            }
        }
        let mut mounted = value.clone();
        mounted["Mounts"] = json!([{"Type":"bind","Source":"/host","Destination":"/run"}]);
        assert!(
            verify_inspect(
                &mounted,
                &name,
                &lease,
                contract().sidecar_image.as_deref().unwrap(),
                expected_id,
                &config().exec_helper_volume
            )
            .is_err()
        );
        for pointer in [
            "/Image",
            "/Config/Labels/dev.open-compute.container.start",
            "/Config/Labels/dev.open-compute.container.contract",
            "/HostConfig/NanoCpus",
            "/HostConfig/Memory",
            "/HostConfig/MemorySwap",
            "/HostConfig/PidsLimit",
            "/HostConfig/StorageOpt/size",
            "/HostConfig/Ulimits/0/Soft",
            "/HostConfig/NetworkMode",
        ] {
            let mut bad = value.clone();
            *bad.pointer_mut(pointer).unwrap() = Value::Null;
            assert!(
                verify_inspect(
                    &bad,
                    &name,
                    &lease,
                    contract().sidecar_image.as_deref().unwrap(),
                    expected_id,
                    &config().exec_helper_volume,
                )
                .is_err()
            );
        }
        for key in [
            "Binds",
            "Devices",
            "Mounts",
            "VolumesFrom",
            "DeviceRequests",
        ] {
            let mut bad = value.clone();
            bad["HostConfig"][key] = json!(["host"]);
            assert!(
                verify_inspect(
                    &bad,
                    &name,
                    &lease,
                    contract().sidecar_image.as_deref().unwrap(),
                    expected_id,
                    &config().exec_helper_volume,
                )
                .is_err()
            );
        }
        let mut bad = value.clone();
        bad["HostConfig"]["Privileged"] = true.into();
        assert!(
            verify_inspect(
                &bad,
                &name,
                &lease,
                contract().sidecar_image.as_deref().unwrap(),
                expected_id,
                &config().exec_helper_volume,
            )
            .is_err()
        );
        let mut bad = value.clone();
        bad["HostConfig"]["PidMode"] = "host".into();
        assert!(
            verify_inspect(
                &bad,
                &name,
                &lease,
                contract().sidecar_image.as_deref().unwrap(),
                expected_id,
                &config().exec_helper_volume,
            )
            .is_err()
        );
        if !sidecar {
            for missing_or_invalid in [None, Some("bad"), Some(lease.lease_key.as_str())] {
                assert!(
                    verify_inspect(
                        &value,
                        &name,
                        &lease,
                        contract().sidecar_image.as_deref().unwrap(),
                        missing_or_invalid,
                        &config().exec_helper_volume,
                    )
                    .is_err()
                );
            }
            for network in [
                format!("container:{}-proxy", lease.lease_key),
                format!("container:{}", "2".repeat(64)),
            ] {
                let mut bad = value.clone();
                bad["HostConfig"]["NetworkMode"] = json!(network);
                assert!(
                    verify_inspect(
                        &bad,
                        &name,
                        &lease,
                        contract().sidecar_image.as_deref().unwrap(),
                        expected_id,
                        &config().exec_helper_volume,
                    )
                    .is_err()
                );
            }
            for drops in [json!([]), json!(["NET_ADMIN"]), Value::Null] {
                let mut bad = value.clone();
                bad["HostConfig"]["CapDrop"] = drops;
                assert!(
                    verify_inspect(
                        &bad,
                        &name,
                        &lease,
                        contract().sidecar_image.as_deref().unwrap(),
                        expected_id,
                        &config().exec_helper_volume,
                    )
                    .is_err()
                );
            }
        }
        let mut bad = value;
        bad["HostConfig"]["CapAdd"] = json!(["SYS_ADMIN"]);
        assert!(
            verify_inspect(
                &bad,
                &name,
                &lease,
                contract().sidecar_image.as_deref().unwrap(),
                expected_id,
                &config().exec_helper_volume,
            )
            .is_err()
        );
    }
}

#[tokio::test]
async fn container_broker_socket_ownership_and_cleanup_are_checked() {
    let root = tempfile::Builder::new()
        .prefix("oc-cb-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = std::fs::canonicalize(root.path())
        .unwrap()
        .join("broker.sock");
    let socket =
        super::socket::BrokerSocket::bind(path.clone(), rustix::process::getuid().as_raw())
            .unwrap();
    super::socket::validate_socket(&path, rustix::process::getuid().as_raw()).unwrap();
    assert!(
        super::socket::BrokerSocket::bind(path.clone(), rustix::process::getuid().as_raw())
            .is_err()
    );
    assert!(super::socket::validate_socket(&path, rustix::process::getuid().as_raw() + 1).is_err());
    drop(socket);
    assert!(!path.exists());
    std::fs::write(&path, b"operator file").unwrap();
    assert!(
        super::socket::BrokerSocket::bind(path.clone(), rustix::process::getuid().as_raw())
            .is_err()
    );
    assert!(super::socket::validate_socket(&path, rustix::process::getuid().as_raw()).is_err());
}

#[test]
fn container_broker_load_stream_never_treats_http_200_errors_as_success() {
    use super::engine::verify_load_response;
    let loaded = format!(
        "{{\"stream\":\"Loaded image ID: sha256:{}\\n\"}}\n",
        "a".repeat(64)
    );
    assert!(verify_load_response(loaded.as_bytes()).is_ok());
    for failed in [
        b"{}".as_slice(),
        b"{\"error\":\"bad image\"}",
        b"{\"errorDetail\":{}}",
        b"invalid",
        b"{\"stream\":\"Loaded image: mutable-tag\"}",
    ] {
        assert!(verify_load_response(failed).is_err());
    }
    let failed_after_load = format!("{loaded}{{\"error\":\"truncated\"}}\n");
    assert!(verify_load_response(failed_after_load.as_bytes()).is_err());
}

#[test]
fn container_broker_public_labels_follow_native_byte_limits() {
    use crate::container_broker::BrokerLabel;
    let mut options = lease().startup;
    options.labels = (0..10)
        .map(|index| BrokerLabel {
            name: format!("label-{index}"),
            value: "é".repeat(32),
        })
        .collect();
    options.labels[0].name = "é".repeat(8);
    options.validate().unwrap();
    let valid = options.clone();
    options.labels.push(BrokerLabel {
        name: "extra".into(),
        value: String::new(),
    });
    assert!(options.validate().is_err());
    for name in [
        "",
        "ééééééééx",
        "bad\nname",
        "bad\tname",
        "bad\0name",
        "bad\u{1f}name",
    ] {
        let mut options = valid.clone();
        options.labels[0].name = name.into();
        assert!(options.validate().is_err());
    }
    for value in [
        "é".repeat(32) + "x",
        "bad\nvalue".into(),
        "bad\tvalue".into(),
        "bad\0value".into(),
        "bad\u{1f}value".into(),
    ] {
        let mut options = valid.clone();
        options.labels[0].value = value;
        assert!(options.validate().is_err());
    }
    let mut options = valid;
    options.labels[0].value.clear();
    options.labels[1].value = "\u{7f}".into();
    options.validate().unwrap();
}

#[test]
fn container_broker_startup_is_frozen_bounded_and_redacted() {
    use crate::container_broker::BrokerLabel;
    let mut lease = lease();
    lease.startup.labels = vec![BrokerLabel {
        name: "tenant.label".into(),
        value: "secret-value".into(),
    }];
    let body = prepare_create(
        &serde_json::to_vec(&create(false)).unwrap(),
        &lease.lease_key,
        &lease,
        contract().sidecar_image.as_deref().unwrap(),
        &lease.startup.environment_variables,
        &config().exec_helper_volume,
    )
    .unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["Labels"]["tenant.label"], "secret-value");
    assert_eq!(body["Env"], json!(["KEY=value"]));
    assert!(!format!("{:?}", lease.startup).contains("secret-value"));
    assert!(!format!("{:?}", lease.startup.labels).contains("secret-value"));
    let mut mismatch = create(false);
    mismatch["Env"] = json!(["KEY=changed"]);
    assert!(
        prepare_create(
            &serde_json::to_vec(&mismatch).unwrap(),
            &lease.lease_key,
            &lease,
            contract().sidecar_image.as_deref().unwrap(),
            &lease.startup.environment_variables,
            &config().exec_helper_volume,
        )
        .is_err()
    );
    mismatch = create(false);
    mismatch["Cmd"] = json!(["changed"]);
    assert!(
        prepare_create(
            &serde_json::to_vec(&mismatch).unwrap(),
            &lease.lease_key,
            &lease,
            contract().sidecar_image.as_deref().unwrap(),
            &lease.startup.environment_variables,
            &config().exec_helper_volume,
        )
        .is_err()
    );
    for variables in [
        vec!["KEY=one".into(), "KEY=two".into()],
        vec!["invalid".into()],
        vec!["=empty".into()],
        vec!["A=\0".into()],
    ] {
        let mut options = lease.startup.clone();
        options.environment_variables = variables;
        assert!(options.validate().is_err());
    }
    for name in ["", "dev.open-compute.lease", "bad\0"] {
        let mut options = lease.startup.clone();
        options.labels[0].name = name.into();
        assert!(options.validate().is_err());
    }
    let mut options = lease.startup.clone();
    options.labels.push(options.labels[0].clone());
    assert!(options.validate().is_err());
    let mut options = lease.startup.clone();
    options.entrypoint = Some(vec!["x".repeat(32_769)]);
    assert!(options.validate().is_err());
}

#[test]
fn container_broker_archive_only_admits_the_native_ca_file() {
    fn archive(path: &str, mode: u32, kind: tar::EntryType) -> Vec<u8> {
        let content = include_bytes!("tests/fixtures/test-ca.pem");
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_ustar();
        header.set_path(path).unwrap();
        header.set_mode(mode);
        header.set_entry_type(kind);
        header.set_size(content.len() as u64);
        header.set_cksum();
        builder.append(&header, content.as_slice()).unwrap();
        builder.into_inner().unwrap()
    }
    let path = "cloudflare/certs/cloudflare-containers-ca.crt";
    super::credentials::validate_ca_archive(&archive(path, 0o644, tar::EntryType::Regular))
        .unwrap();
    for bytes in [
        archive("other.crt", 0o644, tar::EntryType::Regular),
        archive(path, 0o600, tar::EntryType::Regular),
        archive(path, 0o644, tar::EntryType::Directory),
        Vec::new(),
    ] {
        assert!(super::credentials::validate_ca_archive(&bytes).is_err());
    }
    let key = lease().lease_key;
    for (method, path) in [
        (
            Method::GET,
            format!("/containers/{key}/archive?path=%2Fetc"),
        ),
        (
            Method::PUT,
            format!("/containers/{key}-proxy/archive?path=%2Fetc"),
        ),
        (
            Method::PUT,
            format!("/containers/{key}/archive?path=%2Frun"),
        ),
    ] {
        assert!(operation(&method, &path.parse().unwrap()).is_err());
    }
}

#[test]
fn container_broker_reverse_callback_rejects_foreign_or_ambiguous_credentials() {
    let token = "1".repeat(64);
    let request = format!(
        "CONNECT open-compute-probe.invalid:1 HTTP/1.1\r\nX-Proto: tcp\r\nX-Open-Compute-Container-Callback: {token}\r\n\r\n"
    );
    assert!(super::engine::callback::valid_callback(&request, &token));
    for request in [
        request.replace("tcp", "udp"),
        request.replace(&token, &"2".repeat(64)),
        request.replace("open-compute-probe.invalid:1", "foreign.invalid:1"),
        request.replace(
            "\r\n\r\n",
            &format!("\r\nX-Open-Compute-Container-Callback: {token}\r\n\r\n"),
        ),
        request.replace("\r\n\r\n", "\r\nX-Proto: tcp\r\n\r\n"),
        request.replace("\r\n\r\n", "\r\nMalformed-Header\r\n\r\n"),
        request.replace(&token, &"1".repeat(63)),
        "malformed".into(),
    ] {
        assert!(!super::engine::callback::valid_callback(&request, &token));
    }
}

#[test]
fn container_broker_durable_credential_fences_lock_restart_and_corruption() {
    let directory = tempfile::Builder::new()
        .prefix("oc-cbj-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = std::fs::canonicalize(directory.path()).unwrap();
    let key = "a".repeat(64);
    let journal = super::journal::Journal::open(&path, 2).unwrap();
    assert!(super::journal::Journal::open(&path, 2).is_err());
    assert!(journal.begin("invalid").is_err());
    journal.begin(&key).unwrap();
    assert!(journal.begin(&key).is_err());
    assert!(journal.contains(&key).unwrap());
    assert_eq!(journal.keys().unwrap().len(), 1);
    drop(journal);
    let journal = super::journal::Journal::open(&path, 2).unwrap();
    assert!(journal.contains(&key).unwrap());
    journal.clear(&key).unwrap();
    journal.clear(&key).unwrap();
    assert!(journal.keys().unwrap().is_empty());
    drop(journal);
    let pending = path.join(format!("pending-{key}"));
    open_compute_storage::fs::atomic_write(&pending, b"v1\nx").unwrap();
    assert!(super::journal::Journal::open(&path, 2).is_err());
    open_compute_storage::fs::atomic_write(&pending, b"corrupt").unwrap();
    assert!(super::journal::Journal::open(&path, 2).is_err());
    std::fs::remove_file(&pending).unwrap();
    std::os::unix::fs::symlink(path.join("broker.lock"), &pending).unwrap();
    assert!(super::journal::Journal::open(&path, 2).is_err());
}

#[test]
fn image_defaults_and_frozen_overrides_form_exact_effective_startup() {
    use super::startup::EffectiveStartup;
    let mut lease = lease();
    lease
        .startup
        .labels
        .push(crate::container_broker::BrokerLabel {
            name: "shared".into(),
            value: "override".into(),
        });
    let config = json!({"Env":["KEY=image", "IMAGE=value", "IMAGE=ignored", "key=case-sensitive"],
        "Cmd":["default-command"], "Entrypoint":["image-entrypoint"], "User":"1001", "WorkingDir":"/app",
        "Labels":{"shared":"image", "image":"default"}});
    let expected = EffectiveStartup::from_image(&json!({"Config":config}), &lease).unwrap();
    let mut expected_labels = labels(&lease, "application");
    expected_labels["shared"] = json!("override");
    expected_labels["image"] = json!("default");
    let inspect = json!({"Config":{"Env":["KEY=value", "IMAGE=value", "key=case-sensitive"],
        "Cmd":[], "Entrypoint":["/bin/sh"], "User":"1001", "WorkingDir":"/app", "Labels":expected_labels}});
    expected.verify(&inspect, &lease.lease_key).unwrap();
    for (field, altered) in [
        (
            "Env",
            json!([
                "KEY=value",
                "IMAGE=value",
                "key=case-sensitive",
                "UNADMITTED=secret"
            ]),
        ),
        ("Cmd", json!(["changed"])),
        ("Entrypoint", json!([])),
        ("User", json!("0")),
        ("WorkingDir", json!("/")),
        ("Labels", json!({})),
    ] {
        let mut changed = inspect.clone();
        changed["Config"][field] = altered;
        assert!(
            expected.verify(&changed, &lease.lease_key).is_err(),
            "{field}"
        );
    }
    expected
        .verify(&json!({}), &format!("{}-proxy", lease.lease_key))
        .unwrap();
    for command in [None, Some(Vec::new())] {
        lease.startup.entrypoint = command;
        lease.startup.environment_variables.clear();
        let effective = EffectiveStartup::from_image(&json!({"Config":config}), &lease).unwrap();
        let mut inherited = inspect.clone();
        inherited["Config"]["Cmd"] = json!(["default-command"]);
        inherited["Config"]["Entrypoint"] = json!(["image-entrypoint"]);
        inherited["Config"]["Env"] = json!(["KEY=image", "IMAGE=ignored", "key=case-sensitive"]);
        effective.verify(&inherited, &lease.lease_key).unwrap();
    }
    let mut empty = lease.clone();
    empty.startup.labels.clear();
    let effective = EffectiveStartup::from_image(&json!({"Config":{}}), &empty).unwrap();
    effective
        .verify(
            &json!({"Config":{"Labels":labels(&empty,"application")}}),
            &empty.lease_key,
        )
        .unwrap();
    for image in [
        json!({}),
        json!({"Config":[]}),
        json!({"Config":{"Env":"invalid"}}),
        json!({"Config":{"Env":[true]}}),
        json!({"Config":{"Env":[null]}}),
        json!({"Config":{"User":7}}),
        json!({"Config":{"User":"nul\u{0000}"}}),
        json!({"Config":{"Labels":[]}}),
        json!({"Config":{"Labels":{"label":true}}}),
        json!({"Config":{"Env":vec!["VALUE=default";1_025]}}),
        json!({"Config":{"User":"x".repeat(32_769)}}),
        json!({"Config":{"Labels":{("x".repeat(257)):"value"}}}),
    ] {
        assert!(EffectiveStartup::from_image(&image, &empty).is_err());
    }
    let large_defaults: Vec<_> = (0..1_024)
        .map(|index| format!("IMAGE_{index}=value"))
        .collect();
    let mut overrides = empty.clone();
    overrides.startup.environment_variables = vec!["OVERRIDE=value".into()];
    assert!(
        EffectiveStartup::from_image(&json!({"Config":{"Env":large_defaults}}), &overrides)
            .is_err()
    );
    assert!(effective.verify(&json!({}), &empty.lease_key).is_err());
}

#[test]
fn container_broker_qualification_requires_explicit_stop_signal_api() {
    for value in ["1.42", "1.53", "1.54"] {
        assert!(super::engine::supports_stop_signal(
            &json!({"ApiVersion":value})
        ));
    }
    for value in ["1.41", "1.040", "1.", "2.54", "1.54.0", "1.9999", "1.bad"] {
        assert!(!super::engine::supports_stop_signal(
            &json!({"ApiVersion":value})
        ));
    }
    assert!(!super::engine::supports_stop_signal(&json!({})));
    assert!(!super::engine::supports_stop_signal(
        &json!({"ApiVersion":54})
    ));
}
