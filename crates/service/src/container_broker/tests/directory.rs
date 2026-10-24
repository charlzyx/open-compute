//! Frozen archive admission over the real private Unix Broker transport.

use super::*;
use crate::container_broker::{BrokerDirectoryArchive, BrokerDirectoryRestore};
use open_compute_core::containers::ContainerDirectoryPath;
use std::io::Write as _;

fn directory(bytes: &[u8]) -> BrokerDirectoryRestore {
    BrokerDirectoryRestore {
        mount_point: ContainerDirectoryPath::parse("/restored").unwrap(),
        archive: Some(BrokerDirectoryArchive {
            sha256: hex::encode(Sha256::digest(bytes)),
            bytes: bytes.len() as u64,
        }),
    }
}

fn spool(bytes: &[u8]) -> std::fs::File {
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(bytes).unwrap();
    std::io::Seek::rewind(&mut file).unwrap();
    file
}

async fn registered() -> (
    RestartBroker,
    QualifiedContainerProvider,
    BrokerLease,
    Vec<u8>,
) {
    let mut broker = RestartBroker::new().await;
    let provider = broker.start().await;
    let bytes = snapshot::archive(1);
    let mut registration = lease();
    registration.provider_contract_sha256 = provider.contract_sha256().to_owned();
    registration.max_instances = broker.config.max_instances;
    registration.directory_snapshots = vec![directory(&bytes)];
    provider.register(&registration).await.unwrap();
    (broker, provider, registration, bytes)
}

#[test]
fn directory_registration_is_closed_canonical_and_aggregate_bounded() {
    let bytes = snapshot::archive(1);
    let mut registration = lease();
    registration.directory_snapshots = vec![directory(&bytes)];
    assert!(registration.validate(&contract()).is_ok());
    let value = serde_json::to_value(&registration).unwrap();
    for replacement in [
        json!({"mountPoint":"/a"}),
        json!({"mountPoint":"/a","archive":null,"unknown":true}),
        json!({"mountPoint":"/a/../b","archive":null}),
    ] {
        let mut value = value.clone();
        value["directorySnapshots"] = json!([replacement]);
        assert!(serde_json::from_value::<BrokerLease>(value).is_err());
    }
    let original = registration.clone();
    for (path, sha, size) in [
        ("/", "a".repeat(64), 1024),
        ("/.open-compute-exec", "a".repeat(64), 1024),
        ("/.open-compute-exec/helper", "a".repeat(64), 1024),
        ("/a", "bad".into(), 1024),
        ("/a", "a".repeat(64), 0),
        ("/a", "a".repeat(64), contract().max_image_bytes + 1),
    ] {
        registration = original.clone();
        registration.directory_snapshots[0].mount_point =
            ContainerDirectoryPath::parse(path).unwrap();
        registration.directory_snapshots[0].archive = Some(BrokerDirectoryArchive {
            sha256: sha,
            bytes: size,
        });
        assert!(registration.validate(&contract()).is_err());
    }
    registration = original.clone();
    registration
        .directory_snapshots
        .push(registration.directory_snapshots[0].clone());
    assert!(registration.validate(&contract()).is_err());
    registration = original;
    registration.directory_snapshots[0].archive = None;
    assert!(registration.validate(&contract()).is_ok());
}

#[test]
fn directory_mount_identity_preserves_nested_targets_and_rejects_substitution() {
    use crate::container_broker::directory as authority;
    let mut registration = lease();
    let mut nested = directory(b"archive");
    nested.mount_point = ContainerDirectoryPath::parse("/restored/child").unwrap();
    nested.archive = None;
    registration.directory_snapshots = vec![directory(b"archive"), nested];
    registration.validate(&contract()).unwrap();
    let mounts = authority::mounts(&registration, &config().exec_helper_volume);
    let actual: Vec<_> = mounts.as_array().unwrap().iter().rev().map(|mount| {
        json!({"Type":"volume","Name":mount["Source"],"Driver":"local",
            "Destination":mount["Target"],"RW":!mount["ReadOnly"].as_bool().unwrap_or(false),"Propagation":""})
    }).collect();
    let inspection = json!({"HostConfig":{"Mounts":mounts},"Mounts":actual});
    authority::verify_mounts(&inspection, &registration, &config().exec_helper_volume).unwrap();
    let empty = lease();
    assert_eq!(
        authority::mounts(&empty, "helper"),
        crate::container_broker::helper::mount("helper")
    );
    for (pointer, replacement) in [
        ("/Mounts/0/Name", json!("foreign-volume")),
        ("/Mounts/0/RW", json!(false)),
        ("/Mounts/2/RW", json!(true)),
        ("/Mounts/0/Driver", json!("nfs")),
        ("/Mounts/0/Propagation", json!("shared")),
        ("/Mounts/0/Destination", json!("/restored")),
        ("/HostConfig/Mounts/1/VolumeOptions/NoCopy", json!(false)),
    ] {
        let mut changed = inspection.clone();
        *changed.pointer_mut(pointer).unwrap() = replacement;
        assert!(
            authority::verify_mounts(&changed, &registration, &config().exec_helper_volume)
                .is_err(),
            "{pointer}"
        );
    }
    let mut changed = inspection.clone();
    changed["Mounts"].as_array_mut().unwrap().pop();
    assert!(
        authority::verify_mounts(&changed, &registration, &config().exec_helper_volume).is_err()
    );
}

#[test]
fn directory_volume_identity_binds_frozen_content_path_index_and_lease() {
    use crate::container_broker::directory as authority;
    let mut registration = lease();
    registration.directory_snapshots = vec![directory(b"archive")];
    let volume = json!({"Name":authority::volume_name(&registration, 0),"Driver":"local",
        "Scope":"local","Options":null,"Labels":authority::labels(&registration, 0).unwrap()});
    authority::verify_volume(&volume, &registration, 0).unwrap();
    for mutation in 0..6 {
        let mut changed = registration.clone();
        match mutation {
            0 => changed.lease_key = "1".repeat(64),
            1 => changed.owner_key = "2".repeat(64),
            2 => {
                changed.directory_snapshots[0].mount_point =
                    ContainerDirectoryPath::parse("/other").unwrap();
            }
            3 => changed.directory_snapshots[0].archive = None,
            4 => {
                changed.directory_snapshots[0]
                    .archive
                    .as_mut()
                    .unwrap()
                    .sha256 = "3".repeat(64);
            }
            _ => {
                changed.directory_snapshots[0]
                    .archive
                    .as_mut()
                    .unwrap()
                    .bytes += 1;
            }
        }
        assert!(authority::verify_volume(&volume, &changed, 0).is_err());
    }
    assert!(authority::verify_volume(&volume, &registration, 1).is_err());
    for (pointer, replacement) in [
        ("/Driver", json!("nfs")),
        ("/Scope", json!("global")),
        ("/Options", json!({"device":"/host/path"})),
        ("/Labels", json!({})),
    ] {
        let mut changed = volume.clone();
        *changed.pointer_mut(pointer).unwrap() = replacement;
        assert!(authority::verify_volume(&changed, &registration, 0).is_err());
    }
}

#[tokio::test]
async fn directory_engine_archive_range_sends_only_one_frozen_slice() {
    let mut broker = RestartBroker::new().await;
    let _provider = broker.start().await;
    let engine =
        crate::container_broker::engine::Engine::new(&broker.config.engine_socket, "/_ping")
            .await
            .unwrap();
    let first = snapshot::archive(2);
    let second = snapshot::archive(128);
    assert!(second.len() > 65_536);
    let mut bundle = b"prefix".to_vec();
    bundle.extend_from_slice(&first);
    let start = bundle.len() as u64;
    bundle.extend_from_slice(&second);
    let end = bundle.len() as u64;
    bundle.extend_from_slice(b"suffix");
    let path = format!(
        "/containers/{}/archive?path=/open-compute-directories/1",
        lease().lease_key
    );
    let response = engine
        .send_archive(
            Method::PUT,
            &path,
            tokio::fs::File::from_std(spool(&bundle)),
            start..end,
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        *broker.state.directory_uploads.lock().await,
        vec![("/open-compute-directories/1".to_owned(), second.clone())]
    );
    for (method, range) in [
        (Method::POST, 0..end),
        (Method::GET, start..end),
        (Method::PUT, end..end),
        (Method::PUT, start..bundle.len() as u64 + 1),
        (
            Method::PUT,
            std::ops::Range {
                start: end,
                end: start,
            },
        ),
    ] {
        assert!(
            engine
                .send_archive(
                    method,
                    &path,
                    tokio::fs::File::from_std(spool(&bundle)),
                    range
                )
                .await
                .is_err()
        );
    }
    assert_eq!(broker.state.directory_uploads.lock().await.len(), 1);
    let response = engine
        .send_archive(
            Method::POST,
            "/images/load?quiet=true",
            tokio::fs::File::from_std(spool(&second)),
            0..second.len() as u64,
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(*broker.state.imports.lock().await, vec![second]);
    assert_eq!(
        broker
            .client()
            .put(format!("http://localhost{path}"))
            .body(bundle)
            .send()
            .await
            .unwrap()
            .status(),
        503
    );
    broker.retire().await;
}

#[tokio::test]
async fn directory_upload_verifies_digest_size_tar_and_never_overwrites_frozen_inputs() {
    let (mut broker, provider, registration, bytes) = registered().await;
    let mut changed = bytes.clone();
    let offset = changed
        .windows(4)
        .rposition(|window| window == b"data")
        .unwrap();
    changed[offset..offset + 4].copy_from_slice(b"evil");
    open_compute_artifacts::verify_container_archive(changed.as_slice(), changed.len() as u64, 8)
        .unwrap();
    assert!(
        provider
            .stage_directory_restore(&registration, 0, spool(&changed))
            .await
            .is_err()
    );
    for body in [
        vec![0; bytes.len()],
        bytes[..bytes.len() - 1].to_vec(),
        vec![0; bytes.len() + 1],
    ] {
        assert!(
            provider
                .stage_directory_restore(&registration, 0, spool(&body))
                .await
                .is_err()
        );
    }
    provider
        .stage_directory_restore(&registration, 0, spool(&bytes))
        .await
        .unwrap();
    provider
        .stage_directory_restore(&registration, 0, spool(&bytes))
        .await
        .unwrap();
    assert!(
        provider
            .stage_directory_restore(&registration, 0, spool(&changed))
            .await
            .is_err()
    );
    assert!(
        provider
            .stage_directory_restore(&registration, 1, spool(&bytes))
            .await
            .is_err()
    );
    let mut changed_registration = registration.clone();
    changed_registration.directory_snapshots[0].mount_point =
        ContainerDirectoryPath::parse("/other").unwrap();
    assert!(provider.register(&changed_registration).await.is_err());
    let client = broker.client();
    for suffix in ["00", "+0", "-1", "0/extra", "0?extra=true"] {
        assert!(
            client
                .post(format!(
                    "http://localhost/open-compute/v1/leases/{}/directories/{suffix}",
                    registration.lease_key
                ))
                .header("Content-Type", "application/x-tar")
                .body(bytes.clone())
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
                == 503
        );
    }
    assert!(
        client
            .post(format!(
                "http://localhost/containers/create?name={}",
                registration.lease_key
            ))
            .body(serde_json::to_vec(&create(false)).unwrap())
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
            == 503
    );
    provider.terminate(&registration.lease_key).await.unwrap();
    assert!(
        provider
            .stage_directory_restore(&registration, 0, spool(&bytes))
            .await
            .is_err()
    );
    broker.retire().await;
}

#[tokio::test]
async fn directory_upload_restarts_without_trusting_lost_spools_and_rejects_empty_mount_payloads() {
    let (mut broker, provider, registration, bytes) = registered().await;
    provider
        .stage_directory_restore(&registration, 0, spool(&bytes))
        .await
        .unwrap();
    broker.retire().await;
    let provider = broker.start().await;
    provider.register(&registration).await.unwrap();
    provider
        .stage_directory_restore(&registration, 0, spool(&bytes))
        .await
        .unwrap();
    provider.terminate(&registration.lease_key).await.unwrap();
    let mut empty = registration;
    empty.lease_key = "2".repeat(64);
    empty.directory_snapshots[0].archive = None;
    provider.register(&empty).await.unwrap();
    assert!(
        provider
            .stage_directory_restore(&empty, 0, spool(&bytes))
            .await
            .is_err()
    );
    let result = broker
        .client()
        .post(format!(
            "http://localhost/open-compute/v1/leases/{}/directories/0",
            empty.lease_key
        ))
        .header("Content-Type", "application/x-tar")
        .body(bytes)
        .send()
        .await
        .unwrap();
    assert!(result.status().as_u16() == 503);
    provider.terminate(&empty.lease_key).await.unwrap();
    broker.retire().await;
}

async fn directory_proxy(
    broker: &RestartBroker,
    provider: &QualifiedContainerProvider,
    registration: &BrokerLease,
) {
    provider
        .lease_credentials(&registration.lease_key, &"1".repeat(64))
        .await
        .unwrap();
    let client = broker.client();
    let name = format!("{}-proxy", registration.lease_key);
    assert_eq!(
        client
            .post(format!("http://localhost/containers/create?name={name}"))
            .body(serde_json::to_vec(&create(true)).unwrap())
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

async fn directory_main(broker: &RestartBroker, registration: &BrokerLease) -> u16 {
    broker
        .client()
        .post(format!(
            "http://localhost/containers/create?name={}",
            registration.lease_key
        ))
        .body(serde_json::to_vec(&create(false)).unwrap())
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[test]
fn directory_quota_helper_has_closed_mount_authority_and_canonical_upper_path() {
    use crate::container_broker::directory_helper as helper;
    let mut registration = lease();
    registration.directory_snapshots = vec![directory(b"archive")];
    let upper = format!("/docker/overlay2/{}/diff", "1".repeat(64));
    helper::upper_path("/docker", &upper).unwrap();
    for path in [
        format!("{upper}/"),
        upper.replace("/overlay2/", "//overlay2/"),
        upper.replace("/docker/", "/other/"),
        upper.replace("/diff", "/../diff"),
    ] {
        assert!(helper::upper_path("/docker", &path).is_err());
    }
    let request = helper::request(&config(), &registration, &upper).unwrap();
    let mut config_value = request.clone();
    config_value["Labels"]["image.label"] = json!("immutable image default");
    let inspection = json!({"Id":"2".repeat(64),"Name":format!("/{}",helper::name(&registration)),"Image":config().qualification_image,
        "Config":config_value,"HostConfig":request["HostConfig"],"Mounts":helper_input::mounts_from_request(&request["HostConfig"])});
    helper::verify(&inspection, &config(), &registration, &upper).unwrap();
    for (pointer, value) in [
        ("/Config/Cmd", json!(["/bin/sh"])),
        ("/Config/User", json!("1000")),
        ("/HostConfig/NetworkMode", json!("bridge")),
        ("/HostConfig/CapAdd", json!(["SYS_ADMIN"])),
        ("/HostConfig/ReadonlyRootfs", json!(false)),
        ("/HostConfig/Mounts/1/Source", json!("/etc")),
        ("/Mounts/1/RW", json!(true)),
        ("/Mounts/2/Name", json!("foreign")),
    ] {
        let mut changed = inspection.clone();
        *changed.pointer_mut(pointer).unwrap() = value;
        assert!(
            helper::verify(&changed, &config(), &registration, &upper).is_err(),
            "{pointer}"
        );
    }
}

#[tokio::test]
async fn directory_start_restores_indexed_mounts_once_and_adopts_mutated_live_volumes() {
    let mut broker = RestartBroker::new().await;
    let provider = broker.start().await;
    let first = snapshot::archive(1);
    let second = snapshot::archive(2);
    let mut registration = lease();
    registration.provider_contract_sha256 = provider.contract_sha256().to_owned();
    registration.max_instances = 1;
    let mut nested = directory(&second);
    nested.mount_point = ContainerDirectoryPath::parse("/restored/child").unwrap();
    let mut empty = directory(&first);
    empty.mount_point = ContainerDirectoryPath::parse("/empty").unwrap();
    empty.archive = None;
    registration.directory_snapshots = vec![directory(&first), nested, empty];
    provider.register(&registration).await.unwrap();
    directory_proxy(&broker, &provider, &registration).await;
    provider
        .stage_directory_restore(&registration, 0, spool(&first))
        .await
        .unwrap();
    assert_eq!(directory_main(&broker, &registration).await, 503);
    assert!(broker.state.directory_volumes.lock().await.is_empty());
    provider
        .stage_directory_restore(&registration, 1, spool(&second))
        .await
        .unwrap();
    assert_eq!(directory_main(&broker, &registration).await, 201);
    let uploads = broker.state.directory_uploads.lock().await.clone();
    assert_eq!(uploads.len(), 2);
    assert_directory_upload(&uploads[0], &first, 0);
    assert_directory_upload(&uploads[1], &second, 1);
    assert_eq!(broker.state.directory_volumes.lock().await.len(), 3);
    assert!(!broker.state.containers.lock().await.contains_key(
        &crate::container_broker::directory_helper::name(&registration)
    ));
    assert_eq!(
        broker
            .client()
            .post(format!(
                "http://localhost/containers/{}/start",
                registration.lease_key
            ))
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    assert!(
        provider
            .observe(&registration.lease_key)
            .await
            .unwrap()
            .running
    );
    assert!(
        provider
            .observe(&registration.lease_key)
            .await
            .unwrap()
            .directories_prepared
    );
    provider
        .stage_directory_restore(&registration, 0, spool(&first))
        .await
        .unwrap();
    assert_eq!(broker.state.directory_uploads.lock().await.len(), 2);
    // A modified live volume is not rewritten when the immutable lease is adopted again.
    broker
        .state
        .directory_volumes
        .lock()
        .await
        .values_mut()
        .for_each(|volume| {
            volume["CreatedAt"] = json!("retained-live-data");
        });
    broker.retire().await;
    let provider = broker.start().await;
    provider.register(&registration).await.unwrap();
    provider
        .lease_credentials(&registration.lease_key, &"3".repeat(64))
        .await
        .unwrap();
    assert!(
        provider
            .observe(&registration.lease_key)
            .await
            .unwrap()
            .directories_prepared
    );
    provider
        .stage_directory_restore(&registration, 0, spool(&first))
        .await
        .unwrap();
    assert_eq!(directory_main(&broker, &registration).await, 409);
    assert_eq!(broker.state.directory_uploads.lock().await.len(), 2);
    assert!(
        broker
            .state
            .directory_volumes
            .lock()
            .await
            .values()
            .all(|volume| volume["CreatedAt"] == "retained-live-data")
    );
    provider.terminate(&registration.lease_key).await.unwrap();
    assert!(broker.state.directory_volumes.lock().await.is_empty());
    assert!(
        provider
            .observe(&registration.lease_key)
            .await
            .unwrap()
            .cleanup_complete
    );
    broker.retire().await;
}

#[tokio::test]
async fn directory_partial_restore_stays_fenced_until_helper_main_and_volumes_are_removed() {
    for quota_failure in [true, false] {
        let (mut broker, provider, registration, bytes) = registered().await;
        directory_proxy(&broker, &provider, &registration).await;
        provider
            .stage_directory_restore(&registration, 0, spool(&bytes))
            .await
            .unwrap();
        if quota_failure {
            broker
                .state
                .directory_helper_exit
                .store(125, Ordering::SeqCst);
        } else {
            broker
                .state
                .directory_upload_failure
                .store(true, Ordering::SeqCst);
        }
        assert_eq!(directory_main(&broker, &registration).await, 503);
        assert_eq!(
            broker
                .client()
                .post(format!(
                    "http://localhost/containers/{}/start",
                    registration.lease_key
                ))
                .send()
                .await
                .unwrap()
                .status(),
            503
        );
        assert!(
            provider
                .observe(&registration.lease_key)
                .await
                .unwrap()
                .late_operations_fenced
        );
        assert_eq!(broker.state.directory_volumes.lock().await.len(), 1);
        broker.retire().await;
        let provider = broker.start().await;
        assert!(provider.register(&registration).await.is_ok());
        assert!(
            provider
                .lease_credentials(&registration.lease_key, &"4".repeat(64))
                .await
                .is_err()
        );
        provider.adopt_for_cleanup(&registration).await.unwrap();
        let pending = broker
            .config
            .state_dir
            .join(format!("pending-{}", registration.lease_key));
        assert!(pending.is_file());
        provider.terminate(&registration.lease_key).await.unwrap();
        assert!(!pending.exists());
        assert!(broker.state.directory_volumes.lock().await.is_empty());
        assert!(!broker.state.containers.lock().await.contains_key(
            &crate::container_broker::directory_helper::name(&registration)
        ));
        assert!(
            provider
                .observe(&registration.lease_key)
                .await
                .unwrap()
                .cleanup_complete
        );
        broker.retire().await;
    }
}

#[tokio::test]
async fn directory_orphans_reserve_capacity_and_foreign_volumes_are_never_removed() {
    let (mut broker, provider, registration, bytes) = registered().await;
    directory_proxy(&broker, &provider, &registration).await;
    provider
        .stage_directory_restore(&registration, 0, spool(&bytes))
        .await
        .unwrap();
    assert_eq!(directory_main(&broker, &registration).await, 201);
    broker
        .state
        .containers
        .lock()
        .await
        .remove(&registration.lease_key);
    broker
        .state
        .containers
        .lock()
        .await
        .remove(&format!("{}-proxy", registration.lease_key));
    broker.retire().await;
    let provider = broker.start().await;
    let inventory = broker
        .client()
        .get("http://localhost/open-compute/v1/inventory")
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let inventory: Vec<String> = serde_json::from_slice(&inventory).unwrap();
    assert_eq!(inventory, vec![registration.lease_key.clone()]);
    provider.adopt_for_cleanup(&registration).await.unwrap();
    let name = crate::container_broker::directory::volume_name(&registration, 0);
    broker
        .state
        .directory_volumes
        .lock()
        .await
        .get_mut(&name)
        .unwrap()["Labels"]["dev.open-compute.container.owner"] = json!("f".repeat(64));
    assert!(provider.terminate(&registration.lease_key).await.is_err());
    assert!(
        broker
            .state
            .directory_volumes
            .lock()
            .await
            .contains_key(&name)
    );
    assert!(
        !broker
            .state
            .requests
            .lock()
            .await
            .iter()
            .any(|request| request == &format!("DELETE /volumes/{name}"))
    );
    let pending = broker
        .config
        .state_dir
        .join(format!("pending-{}", registration.lease_key));
    assert!(pending.is_file());
    // The fixture owner removes its injected foreign object; Broker never gains that authority.
    broker.state.directory_volumes.lock().await.remove(&name);
    provider.terminate(&registration.lease_key).await.unwrap();
    assert!(!pending.exists());
    assert!(
        provider
            .observe(&registration.lease_key)
            .await
            .unwrap()
            .cleanup_complete
    );
    broker.retire().await;
}

#[tokio::test]
async fn directory_upload_rejects_link_pivots_before_any_engine_archive_write() {
    for target in [
        "../10",
        "/open-compute-directories/10",
        "/open-compute-rootfs",
        "/.open-compute-exec",
    ] {
        let mut archive = tar::Builder::new(Vec::new());
        let mut link = tar::Header::new_ustar();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_mode(0o777);
        link.set_size(0);
        link.set_link_name(target).unwrap();
        link.set_cksum();
        archive
            .append_data(&mut link, "pivot", std::io::empty())
            .unwrap();
        let mut file = tar::Header::new_ustar();
        file.set_mode(0o640);
        file.set_size(6);
        file.set_cksum();
        archive
            .append_data(&mut file, "pivot/escaped", b"escape".as_slice())
            .unwrap();
        let bytes = archive.into_inner().unwrap();
        // The frozen digest is correct: rejection must inspect the directory tree, not a mismatch.
        open_compute_artifacts::verify_container_archive(bytes.as_slice(), bytes.len() as u64, 8)
            .unwrap();
        let mut broker = RestartBroker::new().await;
        let provider = broker.start().await;
        let mut registration = lease();
        registration.provider_contract_sha256 = provider.contract_sha256().to_owned();
        registration.max_instances = broker.config.max_instances;
        registration.directory_snapshots = (0..11)
            .map(|index| {
                let mut entry = directory(&bytes);
                entry.mount_point =
                    ContainerDirectoryPath::parse(&format!("/restore/{index}")).unwrap();
                if index != 1 {
                    entry.archive = None;
                }
                entry
            })
            .collect();
        provider.register(&registration).await.unwrap();
        assert!(
            provider
                .stage_directory_restore(&registration, 1, spool(&bytes))
                .await
                .is_err()
        );
        assert!(broker.state.directory_uploads.lock().await.is_empty());
        assert!(broker.state.directory_volumes.lock().await.is_empty());
        assert!(
            !provider
                .observe(&registration.lease_key)
                .await
                .unwrap()
                .directories_prepared
        );
        provider.terminate(&registration.lease_key).await.unwrap();
        broker.retire().await;
    }
}

#[test]
fn directory_mounts_match_docker_omitted_writable_flag() {
    use crate::container_broker::directory as authority;
    let mut registration = lease();
    registration.directory_snapshots = vec![directory(b"archive")];
    let request = authority::mounts(&registration, &config().exec_helper_volume);
    let mounted = helper_input::mounts_from_request(&json!({"Mounts":request}));
    let mut echoed = request;
    for mount in echoed.as_array_mut().unwrap() {
        if mount.get("ReadOnly") == Some(&json!(false)) {
            mount.as_object_mut().unwrap().remove("ReadOnly");
        }
    }
    let inspection = json!({"HostConfig":{"Mounts":echoed},"Mounts":mounted});
    authority::verify_mounts(&inspection, &registration, &config().exec_helper_volume).unwrap();
    let mut changed = inspection;
    changed["Mounts"][1]["RW"] = json!(false);
    assert!(
        authority::verify_mounts(&changed, &registration, &config().exec_helper_volume).is_err()
    );
}

#[test]
fn directory_quota_mounts_match_docker_omitted_writable_flag() {
    use crate::container_broker::directory_helper as helper;
    let mut registration = lease();
    registration.directory_snapshots = vec![directory(b"archive")];
    let upper = format!("/docker/overlay2/{}/diff", "1".repeat(64));
    let request = helper::request(&config(), &registration, &upper).unwrap();
    let mut echoed = request["HostConfig"].clone();
    for mount in echoed["Mounts"].as_array_mut().unwrap() {
        if mount.get("ReadOnly") == Some(&json!(false)) {
            mount.as_object_mut().unwrap().remove("ReadOnly");
        }
    }
    echoed["LogConfig"]["Config"] = json!({});
    let inspection = json!({"Id":"2".repeat(64),"Name":format!("/{}",helper::name(&registration)),"Image":config().qualification_image,
        "Config":request,"HostConfig":echoed,"Mounts":helper_input::mounts_from_request(&echoed)});
    helper::verify(&inspection, &config(), &registration, &upper).unwrap();
    let mut changed = inspection;
    changed["HostConfig"]["LogConfig"]["Config"] = json!({"labels":"untrusted"});
    assert!(helper::verify(&changed, &config(), &registration, &upper).is_err());
}

#[test]
fn directory_quota_helper_uses_readonly_slave_mount_below_daemon_root() {
    use crate::container_broker::directory_helper as helper;
    let mut registration = lease();
    registration.directory_snapshots = vec![directory(b"archive")];
    let upper = format!("/docker/overlay2/{}/diff", "1".repeat(64));
    let request = helper::request(&config(), &registration, &upper).unwrap();
    assert_eq!(
        request["HostConfig"]["Mounts"][1]["BindOptions"]["Propagation"],
        "rslave"
    );
    assert_eq!(request["HostConfig"]["Mounts"][1]["ReadOnly"], true);
    let inspection = json!({"Id":"2".repeat(64),"Name":format!("/{}",helper::name(&registration)),"Image":config().qualification_image,
        "Config":request,"HostConfig":request["HostConfig"],"Mounts":helper_input::mounts_from_request(&request["HostConfig"])});
    helper::verify(&inspection, &config(), &registration, &upper).unwrap();
    for propagation in ["rprivate", "rshared"] {
        let mut changed = inspection.clone();
        changed["HostConfig"]["Mounts"][1]["BindOptions"]["Propagation"] = json!(propagation);
        assert!(helper::verify(&changed, &config(), &registration, &upper).is_err());
        let mut changed = inspection.clone();
        changed["Mounts"][1]["Propagation"] = json!(propagation);
        assert!(helper::verify(&changed, &config(), &registration, &upper).is_err());
    }
    let mut changed = inspection;
    changed["Mounts"][1]["RW"] = json!(true);
    assert!(helper::verify(&changed, &config(), &registration, &upper).is_err());
}

#[tokio::test]
async fn directory_restore_preserves_parent_symlink_with_unstarted_archive_view() {
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o777);
    header.set_uid(1001);
    header.set_gid(1002);
    header.set_mtime(1_700_000_000);
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_cksum();
    tar.append_link(&mut header, "parent-link", "../elsewhere")
        .unwrap();
    let bytes = tar.into_inner().unwrap();
    assert!(directory_engine::parent_link_escapes(&bytes));
    let mut broker = RestartBroker::new().await;
    let provider = broker.start().await;
    let mut registration = lease();
    registration.provider_contract_sha256 = provider.contract_sha256().to_owned();
    registration.max_instances = 1;
    registration.directory_snapshots = vec![directory(&bytes)];
    provider.register(&registration).await.unwrap();
    directory_proxy(&broker, &provider, &registration).await;
    provider
        .stage_directory_restore(&registration, 0, spool(&bytes))
        .await
        .unwrap();
    assert_eq!(directory_main(&broker, &registration).await, 201);
    let uploads = broker.state.directory_uploads.lock().await.clone();
    assert_eq!(uploads.len(), 1);
    assert_eq!(uploads[0].0, "/");
    assert!(!directory_engine::parent_link_escapes(&uploads[0].1));
    let mut tar = tar::Archive::new(uploads[0].1.as_slice());
    let entry = tar.entries().unwrap().next().unwrap().unwrap();
    assert_eq!(
        entry.path().unwrap().as_ref(),
        std::path::Path::new("open-compute-directories/0/parent-link")
    );
    assert_eq!(
        entry.link_name().unwrap().unwrap().as_ref(),
        std::path::Path::new("../elsewhere")
    );
    assert_eq!(
        (
            entry.header().uid().unwrap(),
            entry.header().gid().unwrap(),
            entry.header().mode().unwrap(),
            entry.header().mtime().unwrap()
        ),
        (1001, 1002, 0o777, 1_700_000_000)
    );
    assert!(!broker.state.containers.lock().await.contains_key(
        &crate::container_broker::directory_helper::name(&registration)
    ));
    provider.terminate(&registration.lease_key).await.unwrap();
    assert!(broker.state.directory_volumes.lock().await.is_empty());
    broker.retire().await;
}

fn assert_directory_upload(upload: &(String, Vec<u8>), original: &[u8], index: usize) {
    use std::io::Read as _;
    assert_eq!(upload.0, "/");
    let mut source = tar::Archive::new(original);
    let mut target = tar::Archive::new(upload.1.as_slice());
    let mut entries = target.entries().unwrap();
    for original in source.entries().unwrap() {
        let mut original = original.unwrap();
        let mut actual = entries.next().unwrap().unwrap();
        assert_eq!(
            actual.path().unwrap().as_ref(),
            PathBuf::from(format!("open-compute-directories/{index}"))
                .join(original.path().unwrap())
        );
        assert_eq!(actual.header().entry_type(), original.header().entry_type());
        assert_eq!(
            (
                actual.header().uid().unwrap(),
                actual.header().gid().unwrap(),
                actual.header().mode().unwrap(),
                actual.header().mtime().unwrap()
            ),
            (
                original.header().uid().unwrap(),
                original.header().gid().unwrap(),
                original.header().mode().unwrap(),
                original.header().mtime().unwrap()
            )
        );
        let mut a = Vec::new();
        let mut b = Vec::new();
        original.read_to_end(&mut a).unwrap();
        actual.read_to_end(&mut b).unwrap();
        assert_eq!(a, b);
    }
    assert!(entries.next().is_none());
}

#[test]
fn directory_archive_receiver_has_no_host_bind_and_must_never_have_started() {
    use crate::container_broker::directory_helper as helper;
    let mut registration = lease();
    registration.directory_snapshots = vec![directory(b"archive")];
    let request = helper::archive_request(&config(), &registration).unwrap();
    assert_eq!(request["HostConfig"]["NetworkMode"], "none");
    assert_eq!(request["HostConfig"]["CapDrop"], json!(["ALL"]));
    assert_eq!(request["HostConfig"]["CapAdd"], json!([]));
    assert!(
        !request["HostConfig"]["Mounts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|mount| mount["Type"] == "bind")
    );
    let value = json!({"Id":"2".repeat(64),"Name":format!("/{}",helper::name(&registration)),"Image":config().qualification_image,"Config":request,"HostConfig":request["HostConfig"],"Mounts":helper_input::mounts_from_request(&request["HostConfig"]),"State":{"Status":"created","Running":false,"Pid":0,"ExitCode":0,"StartedAt":"0001-01-01T00:00:00Z"}});
    helper::verify_archive(&value, &config(), &registration).unwrap();
    for (pointer, replacement) in [
        ("/State/Status", json!("exited")),
        ("/State/Running", json!(true)),
        ("/State/Pid", json!(5)),
        ("/State/ExitCode", json!(125)),
        ("/State/StartedAt", json!("2026-10-10T00:00:00Z")),
        ("/HostConfig/ReadonlyRootfs", json!(true)),
        ("/HostConfig/NetworkMode", json!("bridge")),
        ("/HostConfig/CapAdd", json!(["SYS_ADMIN"])),
        ("/HostConfig/Mounts/0/ReadOnly", json!(false)),
        ("/Mounts/1/Name", json!("foreign")),
        (
            "/Config/Labels/dev.open-compute.container.role",
            json!("directory-helper"),
        ),
    ] {
        let mut changed = value.clone();
        *changed.pointer_mut(pointer).unwrap() = replacement;
        assert!(
            helper::verify_archive(&changed, &config(), &registration).is_err(),
            "{pointer}"
        );
    }
}
