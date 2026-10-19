use super::*;
use sha2::{Digest as _, Sha256};

#[test]
fn registry_tokens_uploads_and_manifest_tags_remain_scoped_and_crash_fenced() {
    let fixture = fixture(2);
    let instance = fixture.authority.instance_id;
    let repository = format!("{instance}/app");
    let repo = ContainerAuthority::new(fixture.storage.db());
    repo.issue_registry_token(instance, &[1; 32], true, true, 500)
        .unwrap();
    repo.issue_registry_token(instance, &[2; 32], false, true, 500)
        .unwrap();
    assert!(
        repo.authenticate_registry(instance, &[1; 32], &repository, true, 499)
            .unwrap()
    );
    assert!(
        repo.authenticate_registry(instance, &[2; 32], &repository, false, 499)
            .unwrap()
    );
    for (hash, path, push, now) in [
        ([1; 32], repository.clone(), true, 500),
        ([2; 32], repository.clone(), true, 1),
        ([3; 32], repository.clone(), false, 1),
        ([1; 32], "another/app".to_owned(), false, 1),
    ] {
        assert!(
            !repo
                .authenticate_registry(instance, &hash, &path, push, now)
                .unwrap()
        );
    }
    assert!(
        repo.issue_registry_token(instance, &[1; 32], true, true, 500)
            .is_err()
    );
    assert!(
        repo.issue_registry_token(instance, &[3; 32], false, false, 500)
            .is_err()
    );
    assert!(
        repo.issue_registry_token(InstanceId::generate(), &[3; 32], true, true, 500)
            .is_err()
    );
    assert!(
        repo.authenticate_registry(InstanceId::generate(), &[1; 32], &repository, true, 1)
            .is_err()
    );
    for path in [
        "another/app".to_owned(),
        format!("{instance}"),
        format!("{instance}/"),
        format!("{instance}/../app"),
        format!("{instance}/UPPER"),
        format!("{instance}/{}", "a".repeat(256)),
    ] {
        assert!(
            repo.create_registry_upload(instance, ResourceId::generate(), &path, 500)
                .is_err()
        );
    }
    let upload = ResourceId::generate();
    repo.create_registry_upload(instance, upload, &repository, 500)
        .unwrap();
    assert_eq!(
        repo.claim_registry_upload(instance, upload, &repository, 1)
            .unwrap(),
        0
    );
    assert!(
        repo.claim_registry_upload(instance, upload, &repository, 1)
            .is_err()
    );
    assert!(repo.finish_registry_append(instance, upload, 1, 0).is_err());
    assert!(
        repo.finish_registry_append(instance, upload, 0, u64::MAX)
            .is_err()
    );
    repo.finish_registry_append(instance, upload, 0, 20)
        .unwrap();
    assert!(
        repo.finish_registry_append(instance, upload, 0, 20)
            .is_err()
    );
    assert_eq!(
        repo.claim_registry_upload(instance, upload, &repository, 1)
            .unwrap(),
        20
    );
    let blob = ContainerRegistryBlob {
        sha256: [4; 32],
        size: 20,
    };
    assert!(
        repo.commit_registry_blob(
            instance,
            upload,
            &repository,
            &ContainerRegistryBlob {
                size: 0,
                ..blob.clone()
            },
            2
        )
        .is_err()
    );
    assert!(
        repo.commit_registry_blob(
            instance,
            upload,
            &repository,
            &ContainerRegistryBlob {
                size: 21,
                ..blob.clone()
            },
            2
        )
        .is_err()
    );
    repo.commit_registry_blob(instance, upload, &repository, &blob, 2)
        .unwrap();
    assert!(
        repo.claim_registry_upload(instance, upload, &repository, 1)
            .is_err()
    );
    let blobs = repo.registry_blobs(instance, &repository).unwrap();
    assert_eq!((blobs[0].sha256, blobs[0].size), (blob.sha256, 20));
    let duplicate = ResourceId::generate();
    repo.create_registry_upload(instance, duplicate, &repository, 500)
        .unwrap();
    repo.claim_registry_upload(instance, duplicate, &repository, 1)
        .unwrap();
    repo.finish_registry_append(instance, duplicate, 0, 21)
        .unwrap();
    repo.claim_registry_upload(instance, duplicate, &repository, 1)
        .unwrap();
    assert!(
        repo.commit_registry_blob(
            instance,
            duplicate,
            &repository,
            &ContainerRegistryBlob {
                size: 21,
                ..blob.clone()
            },
            2
        )
        .is_err()
    );
    assert!(
        repo.claim_registry_upload(instance, duplicate, &repository, 1)
            .is_err()
    );
    let expired = ResourceId::generate();
    repo.create_registry_upload(instance, expired, &repository, 3)
        .unwrap();
    assert!(
        repo.claim_registry_upload(instance, expired, &repository, 3)
            .is_err()
    );
    let bytes=serde_json::to_vec(&serde_json::json!({"config":{"digest":format!("sha256:{}",hex::encode(blob.sha256)),"size":blob.size},"layers":[]})).unwrap();
    let digest: [u8; 32] = Sha256::digest(&bytes).into();
    let mut admitted = image(12);
    admitted.manifest_sha256 = digest;
    repo.admit_image(instance, &admitted).unwrap();
    assert!(
        repo.publish_registry_manifest(instance, &repository, &[0; 32], &bytes)
            .is_err()
    );
    repo.publish_registry_manifest(instance, &repository, &digest, &bytes)
        .unwrap();
    repo.publish_registry_manifest(instance, &repository, &digest, &bytes)
        .unwrap();
    assert_eq!(
        repo.registry_manifest(instance, &repository, &digest)
            .unwrap(),
        bytes
    );
    assert!(
        repo.registry_manifest(instance, &repository, &[0; 32])
            .is_err()
    );
    for tag in ["", ".invalid", "-invalid", "with/slash", "with spaces"] {
        assert!(
            repo.publish_registry_tag(instance, &repository, tag, &digest, 5)
                .is_err()
        );
    }
    assert!(repo.registry_tag(instance, &repository, "missing").is_err());
    repo.publish_registry_tag(instance, &repository, "release-v1.0", &digest, 5)
        .unwrap();
    assert_eq!(
        repo.registry_tag(instance, &repository, "release-v1.0")
            .unwrap(),
        digest
    );
    assert!(
        repo.publish_registry_tag(instance, &repository, "release-v1.0", &[0; 32], 6)
            .is_err()
    );
    assert_eq!(
        repo.registry_tag(instance, &repository, "release-v1.0")
            .unwrap(),
        digest
    );
    let trigger: String = fixture
        .storage
        .db()
        .with_read(|conn| {
            conn.query_row(
                "SELECT sql FROM sqlite_master WHERE name='container_registry_manifest_immutable'",
                [],
                |row| row.get(0),
            )
            .map_err(|_| invariant())
        })
        .unwrap();
    fixture
        .storage
        .db()
        .with_immediate(|tx| {
            tx.execute_batch("DROP TRIGGER container_registry_manifest_immutable")
                .map_err(|_| invariant())?;
            tx.execute(
                "UPDATE container_registry_manifests SET content=x'616263'",
                [],
            )
            .map_err(|_| invariant())?;
            Ok(())
        })
        .unwrap();
    assert!(
        repo.registry_manifest(instance, &repository, &digest)
            .is_err()
    );
    fixture
        .storage
        .db()
        .with_immediate(|tx| tx.execute_batch(&trigger).map_err(|_| invariant()))
        .unwrap();
    let namespace = fixture.authority.namespace_resource_id;
    let root = fixture.directory.path().join("data");
    drop(fixture.storage);
    let reopened = PlatformStorage::bootstrap(&configuration(&root), &SystemClock).unwrap();
    let repo = ContainerAuthority::new(reopened.db());
    assert_eq!(repo.registry_blobs(instance, &repository).unwrap().len(), 1);
    assert!(
        repo.claim_registry_upload(instance, duplicate, &repository, 1)
            .is_err()
    );
    assert!(
        repo.authenticate_registry(instance, &[1; 32], &repository, true, 1)
            .unwrap()
    );
    let app = repo
        .application_for_namespace(instance, namespace, "container-app")
        .unwrap();
    let lease = repo
        .reserve(
            &fixture.authority,
            &"d".repeat(64),
            &execution(
                &reopened,
                &fixture.authority,
                &fixture.spec,
                &"d".repeat(64),
            ),
            10,
            10,
        )
        .unwrap();
    assert_eq!(repo.application_leases(instance, app.id).unwrap().len(), 1);
    let mut stale = fixture.authority;
    stale.route_generation += 1;
    assert!(
        repo.begin_authorized_stop(&stale, lease.identity, &lease.opaque_engine_key, 11)
            .is_err()
    );
    assert!(
        repo.begin_authorized_stop(&fixture.authority, lease.identity, &"e".repeat(64), 11)
            .is_err()
    );
    repo.begin_authorized_stop(
        &fixture.authority,
        lease.identity,
        &lease.opaque_engine_key,
        11,
    )
    .unwrap();
    assert_eq!(
        repo.lease(instance, lease.identity)
            .unwrap()
            .stop_requested_at_ms,
        Some(11)
    );
}

#[test]
fn registry_mounts_and_expiration_release_only_abandoned_staging() {
    let fixture = fixture(4);
    let instance = fixture.storage.identity().instance_id;
    let repo = ContainerAuthority::new(fixture.storage.db());
    let source = format!("{instance}/source");
    let target = format!("{instance}/target");
    let id = ResourceId::generate();
    let blob = ContainerRegistryBlob {
        sha256: [7; 32],
        size: 24,
    };
    repo.issue_registry_token(instance, &[8; 32], true, true, 20)
        .unwrap();
    repo.create_registry_upload(instance, id, &source, 10)
        .unwrap();
    assert_eq!(repo.registry_upload_ids(instance).unwrap(), vec![id]);
    assert_eq!(
        repo.claim_registry_upload(instance, id, &source, 1)
            .unwrap(),
        0
    );
    repo.finish_registry_append(instance, id, 0, 24).unwrap();
    repo.claim_registry_upload(instance, id, &source, 2)
        .unwrap();
    repo.commit_registry_blob(instance, id, &source, &blob, 3)
        .unwrap();
    assert!(
        repo.mount_registry_blob(instance, &source, &target, &blob.sha256, 4)
            .unwrap()
    );
    assert_eq!(repo.registry_blobs(instance, &target).unwrap()[0].size, 24);
    assert!(
        !repo
            .mount_registry_blob(instance, &source, &target, &[1; 32], 5)
            .unwrap()
    );
    assert!(
        repo.mount_registry_blob(instance, "foreign/source", &target, &blob.sha256, 5)
            .is_err()
    );
    let abandoned = ResourceId::generate();
    repo.create_registry_upload(instance, abandoned, &source, 10)
        .unwrap();
    assert_eq!(
        repo.expire_registry_staging(instance, 10).unwrap(),
        vec![abandoned]
    );
    assert_eq!(repo.registry_blobs(instance, &source).unwrap().len(), 1);
    assert!(
        repo.authenticate_registry(instance, &[8; 32], &source, true, 11)
            .unwrap()
    );
    assert!(
        repo.expire_registry_staging(instance, 900_003)
            .unwrap()
            .is_empty()
    );
    assert!(repo.registry_blobs(instance, &source).unwrap().is_empty());
    assert!(repo.registry_blobs(instance, &target).unwrap().is_empty());
    assert!(
        !repo
            .authenticate_registry(instance, &[8; 32], &source, true, 11)
            .unwrap()
    );
}
