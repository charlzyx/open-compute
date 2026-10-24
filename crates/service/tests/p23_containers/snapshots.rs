//! Public snapshot handles restore across objects and daemon restarts in one application.

use super::*;
use open_compute_artifacts::{
    ARTIFACT_KEY_VERSION, ArtifactRef, ContainerImageLimits, GetOptions, MapEnv, ObjectBackend,
    ObjectKey, resolve_s3_credentials_with, verify_container_image_archive,
};
use open_compute_storage::containers::{
    CONTAINER_SNAPSHOT_RETENTION_MS, ContainerSnapshot, ContainerSnapshotState,
};
use sha2::{Digest as _, Sha256};
use std::io::{Read, Seek as _};
use std::path::Path;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

pub(super) struct SavedSnapshot {
    pub(super) id: ResourceId,
    artifact: [u8; 32],
    artifact_size: u64,
    pub(super) image: [u8; 32],
    pub(super) unnamed_image: [u8; 32],
    image_size: u64,
    source: ContainerLease,
    expires_at_ms: i64,
}

fn artifact(snapshot: &ContainerSnapshot) -> ArtifactRef {
    ArtifactRef::new(
        ARTIFACT_KEY_VERSION,
        &hex::encode(snapshot.artifact_sha256.unwrap()),
        snapshot.artifact_size.unwrap(),
    )
    .unwrap()
}

pub(super) async fn persisted_artifact(
    config: &PlatformConfig,
    snapshot: &ContainerSnapshot,
) -> fs::File {
    let artifact = artifact(snapshot);
    let s3 = config.object_storage.as_s3().unwrap();
    let credentials = resolve_s3_credentials_with(s3, &MapEnv::new()).unwrap();
    let backend = ObjectBackend::connect_s3(s3, &credentials, artifact.size()).unwrap();
    let key = ObjectKey::new(artifact.physical_key(backend.prefix())).unwrap();
    tokio::time::timeout(Duration::from_secs(180), async {
        let object = backend.get(&key, GetOptions::default()).await.unwrap();
        assert_eq!(object.metadata.size, artifact.size());
        assert!(object.range.is_none());
        let mut reader = object.body.into_async_read();
        let file = tempfile::tempfile_in(config.data.path.parent().unwrap()).unwrap();
        let mut file = tokio::fs::File::from_std(file);
        let mut buffer = vec![0_u8; 65_536];
        let mut digest = Sha256::new();
        let mut size = 0_u64;
        loop {
            let count = reader.read(&mut buffer).await.unwrap();
            if count == 0 {
                break;
            }
            size = size.checked_add(count as u64).unwrap();
            assert!(size <= artifact.size());
            digest.update(&buffer[..count]);
            file.write_all(&buffer[..count]).await.unwrap();
        }
        assert_eq!(size, artifact.size());
        assert_eq!(
            <[u8; 32]>::from(digest.finalize()),
            *artifact.sha256_bytes()
        );
        file.flush().await.unwrap();
        let mut file = file.into_std().await;
        file.rewind().unwrap();
        file
    })
    .await
    .expect("persisted snapshot artifact must be read and verified within the bounded budget")
}

async fn persisted_image_size(
    config: &PlatformConfig,
    instance: InstanceId,
    snapshot: &ContainerSnapshot,
    provider: &QualifiedContainerProvider,
) -> u64 {
    let image = {
        let db = ControlDb::open_readonly_wal_aware(&config.data.path.join("control.sqlite"), 1000)
            .unwrap();
        ContainerAuthority::new(&db)
            .image(instance, &snapshot.image_sha256.unwrap())
            .unwrap()
    };
    assert_ne!(snapshot.image_sha256, Some(snapshot.source_image_sha256));
    let image_size = snapshot.image_size.unwrap();
    let reference = format!("sha256:{}", hex::encode(image.config_sha256));
    let diff_ids = image
        .diff_ids
        .iter()
        .map(|digest| format!("sha256:{}", hex::encode(digest)))
        .collect::<Vec<_>>();
    assert_eq!(
        provider.image_size(&reference, &diff_ids).await.unwrap(),
        Some(image_size)
    );
    image_size
}

fn normalized_archive_path(path: &[u8]) -> Vec<u8> {
    path.split(|byte| *byte == b'/')
        .filter(|part| !part.is_empty() && *part != b".")
        .collect::<Vec<_>>()
        .join(&b'/')
}

fn rootfs_excludes_helper(reader: impl Read) -> bool {
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let kind = entry.header().entry_type();
        if normalized_archive_path(&entry.path_bytes()) == b".open-compute-exec/helper"
            && !kind.is_dir()
            || kind.is_hard_link()
                && entry.link_name_bytes().is_some_and(|path| {
                    normalized_archive_path(&path) == b".open-compute-exec/helper"
                })
        {
            return false;
        }
        std::io::copy(&mut entry, &mut std::io::sink()).unwrap();
    }
    true
}

async fn restored_artifact_excludes_helper(
    config: &PlatformConfig,
    instance: InstanceId,
    snapshot: &ContainerSnapshot,
) {
    let artifact = artifact(snapshot);
    let db =
        ControlDb::open_readonly_wal_aware(&config.data.path.join("control.sqlite"), 1000).unwrap();
    let image = ContainerAuthority::new(&db)
        .image(instance, &snapshot.image_sha256.unwrap())
        .unwrap();
    drop(db);
    assert_eq!(image.artifact_sha256, *artifact.sha256_bytes());
    assert_eq!(image.artifact_size, artifact.size());
    // A disposable cache miss must not bypass verification of the authoritative archive.
    let file = persisted_artifact(config, snapshot).await;
    tokio::task::spawn_blocking(move || {
        let verified = verify_container_image_archive(
            &file,
            &artifact,
            image.manifest_sha256,
            &image.architecture,
            ContainerImageLimits {
                json_bytes: 1 << 20,
                compressed_bytes: image.compressed_bytes,
                unpacked_bytes: image.unpacked_bytes,
                layers: 1,
                entries: 100_000,
            },
        )
        .unwrap();
        assert_eq!(verified.layers().len(), 1);
        let layer = &verified.layers()[0];
        assert_eq!(verified.diff_ids(), image.diff_ids.as_slice());
        let path = format!("blobs/sha256/{}", hex::encode(layer.sha256));
        let mut archive = tar::Archive::new(&file);
        let mut found = 0;
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            if entry.path_bytes().as_ref() == path.as_bytes() {
                assert!(entry.header().entry_type().is_file());
                assert_eq!(entry.size(), layer.bytes);
                assert!(rootfs_excludes_helper(flate2::read::MultiGzDecoder::new(
                    &mut entry
                )));
                found += 1;
            }
        }
        assert_eq!(found, 1);
    })
    .await
    .unwrap();
}

pub(super) fn snapshot(data: &Path, instance: InstanceId, id: ResourceId) -> ContainerSnapshot {
    let db = ControlDb::open_readonly_wal_aware(&data.join("control.sqlite"), 1000).unwrap();
    ContainerAuthority::new(&db)
        .snapshots(instance)
        .unwrap()
        .into_iter()
        .find(|snapshot| snapshot.id == id)
        .unwrap()
}

fn declared_images(data: &Path, instance: InstanceId) -> Value {
    let db = ControlDb::open_readonly_wal_aware(&data.join("control.sqlite"), 1000).unwrap();
    let declarations = ContainerAuthority::new(&db)
        .version_declarations(instance, cf::active_version(data, instance))
        .unwrap();
    let named = declarations
        .into_iter()
        .find(|declaration| declaration.application_name == "containers-named-fixture")
        .unwrap();
    assert_eq!(named.images.len(), 2);
    serde_json::to_value(named.images).unwrap()
}

async fn restored(
    data: &Path,
    instance: InstanceId,
    application: ResourceId,
    id: ResourceId,
) -> ContainerLease {
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            if let Some(lease) = leases(data, instance, application)
                .into_iter()
                .find(|lease| {
                    lease.state == ContainerLeaseState::Running
                        && lease.startup.snapshot_id == Some(id)
                })
            {
                break lease;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("snapshot restore requires positive Running authority")
}

pub(super) async fn capture_and_cross_restore(
    client: &http::Client,
    public: &str,
    host: &str,
    config: &Path,
    instance: InstanceId,
    application: ResourceId,
    provider: &QualifiedContainerProvider,
) -> SavedSnapshot {
    let config = PlatformConfig::from_toml_str(&fs::read_to_string(config).unwrap()).unwrap();
    let data = &config.data.path;
    assert!(provider.supports_snapshots());
    assert_eq!(
        tenant(
            client,
            public,
            host,
            "/named/surface?object=snapshot-source"
        )
        .await,
        json!({"snapshotContainer":"function","snapshotDirectory":"undefined"})
    );
    assert_eq!(
        tenant(
            client,
            public,
            host,
            "/named/snapshot-write?object=snapshot-source&value=saved"
        )
        .await,
        json!({"written":"saved"})
    );
    let source = leases(data, instance, application)
        .into_iter()
        .find(|lease| lease.state == ContainerLeaseState::Running)
        .unwrap();
    let before = provider.observe(&source.opaque_engine_key).await.unwrap();
    let handle = tenant(
        client,
        public,
        host,
        "/named/snapshot?object=snapshot-source",
    )
    .await;
    assert_eq!(handle.as_object().unwrap().len(), 3);
    assert_eq!(handle["name"], "fixture-saved");
    assert!(handle["size"].as_u64().unwrap() > 0);
    let id: ResourceId = handle["id"].as_str().unwrap().parse().unwrap();
    let saved = snapshot(data, instance, id);
    assert_eq!(saved.application_id, application);
    assert_eq!(saved.source.identity, source.identity);
    assert_eq!(saved.state, ContainerSnapshotState::Ready);
    let image_size = persisted_image_size(&config, instance, &saved, provider).await;
    assert_eq!(Some(image_size), handle["size"].as_u64());
    persisted_artifact(&config, &saved).await;
    assert_eq!(
        saved.expires_at_ms,
        saved.captured_at_ms.unwrap() + CONTAINER_SNAPSHOT_RETENTION_MS
    );
    let after = provider.observe(&source.opaque_engine_key).await.unwrap();
    assert!(after.running && !after.late_operations_fenced);
    assert_eq!(after.start_identity, before.start_identity);
    let unnamed_handle = tenant(
        client,
        public,
        host,
        "/named/snapshot-default?object=snapshot-source",
    )
    .await;
    assert_eq!(unnamed_handle.as_object().unwrap().len(), 2);
    assert!(unnamed_handle.get("name").is_none());
    let unnamed_id: ResourceId = unnamed_handle["id"].as_str().unwrap().parse().unwrap();
    assert_ne!(unnamed_id, id);
    let unnamed = snapshot(data, instance, unnamed_id);
    assert_eq!(unnamed.application_id, application);
    assert_eq!(unnamed.source.identity, source.identity);
    assert_eq!(unnamed.state, ContainerSnapshotState::Ready);
    assert!(unnamed.name.is_none());
    let unnamed_size = persisted_image_size(&config, instance, &unnamed, provider).await;
    assert!(unnamed_size > 0);
    assert_eq!(unnamed_handle["size"].as_u64(), Some(unnamed_size));
    persisted_artifact(&config, &unnamed).await;
    assert_eq!(
        tenant(
            client,
            public,
            host,
            &format!("/named/restore?object=snapshot-default-target&id={unnamed_id}")
        )
        .await,
        json!({"value":"saved","mode":416,"volatile":false})
    );
    let unnamed_target = restored(data, instance, application, unnamed_id).await;
    assert_ne!(unnamed_target.identity.object_id, source.identity.object_id);
    restored_artifact_excludes_helper(&config, instance, &unnamed).await;
    assert_eq!(
        tenant(
            client,
            public,
            host,
            "/named/stop?object=snapshot-default-target"
        )
        .await["running"],
        false
    );
    assert_eq!(
        tenant(
            client,
            public,
            host,
            "/named/snapshot-write?object=snapshot-source&value=mutated"
        )
        .await,
        json!({"written":"mutated"})
    );
    assert_eq!(
        tenant(
            client,
            public,
            host,
            "/named/snapshot-state?object=snapshot-source"
        )
        .await,
        json!({"value":"mutated","mode":416,"volatile":true})
    );
    assert_eq!(
        tenant(
            client,
            public,
            host,
            &format!("/named/restore?object=snapshot-target&id={id}")
        )
        .await,
        json!({"value":"saved","mode":416,"volatile":false})
    );
    let target = restored(data, instance, application, id).await;
    assert_eq!(
        tenant(
            client,
            public,
            host,
            "/named/inspection?object=snapshot-target"
        )
        .await,
        json!({
            "running": true,
            "info": {"image": "", "labels": {"purpose": "containers-fixture"}},
            "images": declared_images(data, instance),
        })
    );
    // Full archive verification can outlast the container's default inactivity timeout.
    restored_artifact_excludes_helper(&config, instance, &saved).await;
    assert_ne!(target.identity.object_id, source.identity.object_id);
    assert_eq!(target.application_id, source.application_id);
    assert_eq!(Some(target.effective_image_sha256), saved.image_sha256);
    assert_ne!(target.effective_image_sha256, source.effective_image_sha256);
    let renewed = snapshot(data, instance, id);
    assert!(renewed.last_restored_at_ms.unwrap() >= saved.captured_at_ms.unwrap());
    assert!(renewed.last_restored_at_ms.unwrap() <= target.updated_at_ms);
    assert_eq!(
        renewed.expires_at_ms,
        renewed.last_restored_at_ms.unwrap() + CONTAINER_SNAPSHOT_RETENTION_MS
    );
    for object in ["snapshot-source", "snapshot-target"] {
        assert_eq!(
            tenant(
                client,
                public,
                host,
                &format!("/named/stop?object={object}")
            )
            .await["running"],
            false
        );
    }
    let states = leases(data, instance, application);
    assert!(
        states
            .iter()
            .all(|lease| lease.state == ContainerLeaseState::Stopped)
    );
    SavedSnapshot {
        id,
        artifact: saved.artifact_sha256.unwrap(),
        artifact_size: saved.artifact_size.unwrap(),
        image: saved.image_sha256.unwrap(),
        unnamed_image: unnamed.image_sha256.unwrap(),
        image_size,
        source,
        expires_at_ms: saved.expires_at_ms,
    }
}

pub(super) async fn restore_after_restart(
    client: &http::Client,
    public: &str,
    host: &str,
    config: &Path,
    instance: InstanceId,
    application: ResourceId,
    saved: SavedSnapshot,
) {
    let config = PlatformConfig::from_toml_str(&fs::read_to_string(config).unwrap()).unwrap();
    let data = &config.data.path;
    let before = snapshot(data, instance, saved.id);
    assert_eq!(before.state, ContainerSnapshotState::Ready);
    assert_eq!(before.artifact_sha256, Some(saved.artifact));
    assert_eq!(before.artifact_size, Some(saved.artifact_size));
    assert_eq!(before.image_sha256, Some(saved.image));
    assert_eq!(before.image_size, Some(saved.image_size));
    let provider = QualifiedContainerProvider::preflight(config.containers.as_ref().unwrap())
        .await
        .unwrap();
    assert_eq!(
        persisted_image_size(&config, instance, &before, &provider).await,
        saved.image_size
    );
    persisted_artifact(&config, &before).await;
    assert_eq!(
        tenant(
            client,
            public,
            host,
            &format!("/named/restore?object=snapshot-persisted&id={}", saved.id)
        )
        .await,
        json!({"value":"saved","mode":416,"volatile":false})
    );
    let lease = restored(data, instance, application, saved.id).await;
    assert_ne!(lease.identity.object_id, saved.source.identity.object_id);
    let renewed = snapshot(data, instance, saved.id);
    assert_eq!(renewed.image_sha256, Some(saved.image));
    assert_eq!(renewed.image_size, Some(saved.image_size));
    restored_artifact_excludes_helper(&config, instance, &renewed).await;
    assert!(renewed.expires_at_ms >= saved.expires_at_ms);
    assert_eq!(
        renewed.expires_at_ms,
        renewed.last_restored_at_ms.unwrap() + CONTAINER_SNAPSHOT_RETENTION_MS
    );
    assert_eq!(
        tenant(
            client,
            public,
            host,
            "/named/stop?object=snapshot-persisted"
        )
        .await["running"],
        false
    );
    let ordinary = format!(
        "snapshot-private@sha256:{}",
        hex::encode(renewed.image_sha256.unwrap())
    );
    let count = leases(data, instance, application).len();
    assert_eq!(
        tenant(
            client,
            public,
            host,
            &format!("/named/restore-image?object=snapshot-rejected&image={ordinary}")
        )
        .await,
        json!({"denied":true})
    );
    assert_eq!(
        leases(data, instance, application).len(),
        count,
        "private snapshot images cannot enter ordinary image authority"
    );
    assert!(
        leases(data, instance, application)
            .iter()
            .all(|lease| lease.state == ContainerLeaseState::Stopped)
    );
}

#[test]
fn snapshot_rootfs_excludes_helper_entities_and_hardlink_references() {
    fn archive(path: &str, kind: tar::EntryType, link: Option<&str>) -> Vec<u8> {
        let mut archive = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_ustar();
        header.set_entry_type(kind);
        header.set_mode(0o755);
        header.set_size(0);
        if let Some(link) = link {
            header.set_link_name(link).unwrap();
        }
        header.set_cksum();
        archive
            .append_data(&mut header, path, std::io::empty())
            .unwrap();
        archive.into_inner().unwrap()
    }

    for (path, kind, link) in [
        (
            "./.open-compute-exec/helper/",
            tar::EntryType::Directory,
            None,
        ),
        ("tmp/saved", tar::EntryType::Regular, None),
        ("tmp/alias", tar::EntryType::Link, Some("tmp/saved")),
    ] {
        assert!(rootfs_excludes_helper(archive(path, kind, link).as_slice()));
    }
    for (path, kind, link) in [
        (".open-compute-exec/helper", tar::EntryType::Regular, None),
        ("./.open-compute-exec/helper", tar::EntryType::Regular, None),
        (
            "./.open-compute-exec/helper",
            tar::EntryType::Symlink,
            Some("/operator/helper"),
        ),
        (
            ".open-compute-exec/helper",
            tar::EntryType::Link,
            Some("tmp/saved"),
        ),
        (
            "tmp/alias",
            tar::EntryType::Link,
            Some("./.open-compute-exec/helper"),
        ),
    ] {
        assert!(!rootfs_excludes_helper(
            archive(path, kind, link).as_slice()
        ));
    }
}
