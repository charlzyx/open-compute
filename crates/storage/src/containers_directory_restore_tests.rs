use super::*;
use open_compute_core::containers::ContainerDirectoryPath;

fn mount(id: Option<ResourceId>, target: &str) -> ContainerDirectoryRestore {
    ContainerDirectoryRestore {
        snapshot_id: id,
        mount_point: ContainerDirectoryPath::parse(target).unwrap(),
    }
}

fn directory_image() -> ContainerImage {
    let mut image = image(22);
    image.diff_ids.truncate(1);
    image.unpacked_bytes = 1502;
    image
}

fn ready_directory(fixture: &Fixture, source: &ContainerSnapshotSource) -> ContainerSnapshot {
    let repo = ContainerAuthority::new(fixture.storage.db());
    let pending = repo
        .reserve_snapshot(
            &fixture.authority,
            &NewContainerSnapshot {
                id: ResourceId::generate(),
                source: source.clone(),
                name: None,
                directory: Some(ContainerDirectoryPath::parse("/app/data").unwrap()),
                reserved_bytes: 4096,
            },
            limits(),
            40,
        )
        .unwrap();
    repo.stage_snapshot_artifact(
        fixture.authority.instance_id,
        pending.id,
        source,
        &directory_image(),
    )
    .unwrap();
    repo.finish_snapshot_capture(&fixture.authority, pending.id, source, 1502, 41)
        .unwrap()
}

#[test]
fn directory_restore_reservation_survives_restart_and_holds_artifacts_until_cleanup() {
    let fixture = fixture(4);
    let source = source(&fixture);
    let snapshot = ready_directory(&fixture, &source);
    let target = other_object(&fixture, 63);
    let key = "c".repeat(64);
    let mut spec = execution(&fixture.storage, &target, &fixture.spec, &key);
    spec.directory_snapshots = vec![mount(Some(snapshot.id), "/restored"), mount(None, "/empty")];
    let repo = ContainerAuthority::new(fixture.storage.db());
    let lease = repo.reserve(&target, &key, &spec, 10, 44).unwrap();
    assert_eq!(lease.directory_snapshots, spec.directory_snapshots);
    assert_eq!(
        repo.snapshots(target.instance_id).unwrap()[0].expires_at_ms,
        snapshot.expires_at_ms
    );
    // A retry cannot replace, add or remove a reserved clone.
    let mut changed = spec.clone();
    changed.directory_snapshots[0].mount_point = ContainerDirectoryPath::parse("/other").unwrap();
    assert!(repo.reserve(&target, &key, &changed, 10, 45).is_err());
    changed.directory_snapshots.clear();
    assert!(repo.reserve(&target, &key, &changed, 10, 45).is_err());
    assert_eq!(repo.reserve(&target, &key, &spec, 10, 45).unwrap(), lease);
    let start = "d".repeat(64);
    repo.mark_starting(lease.identity, &key, 46).unwrap();
    repo.observe_running(lease.identity, &observe(&key, &start, &fixture.spec), 47)
        .unwrap();
    let renewed = repo.snapshots(target.instance_id).unwrap().remove(0);
    assert_eq!(renewed.last_restored_at_ms, Some(47));
    assert_eq!(renewed.expires_at_ms, 47 + CONTAINER_SNAPSHOT_RETENTION_MS);
    repo.lose_all(target.instance_id, 48).unwrap();
    let root = fixture.directory.path().join("data");
    drop(fixture.storage);
    let storage = PlatformStorage::bootstrap(&configuration(&root), &SystemClock).unwrap();
    let repo = ContainerAuthority::new(storage.db());
    let recovered = repo.lease(target.instance_id, lease.identity).unwrap();
    assert_eq!(recovered.directory_snapshots, spec.directory_snapshots);
    assert_eq!(recovered.state, ContainerLeaseState::Lost);
    let expired = renewed.expires_at_ms + 1;
    assert_eq!(
        repo.retire_snapshots(target.instance_id, expired).unwrap(),
        0
    );
    let inputs = repo.directory_restore_inputs(&target, &key).unwrap();
    assert_eq!(
        inputs,
        vec![
            (spec.directory_snapshots[0].clone(), Some(directory_image())),
            (spec.directory_snapshots[1].clone(), None),
        ]
    );
    assert!(
        repo.directory_snapshot_for_restore(&target, snapshot.id, expired)
            .is_err()
    );
    assert!(
        repo.directory_restore_inputs(&target, &"a".repeat(64))
            .is_err()
    );
    let stale = ContainerObjectAuthority {
        route_generation: target.route_generation + 1,
        ..target
    };
    assert!(repo.directory_restore_inputs(&stale, &key).is_err());
    repo.begin_application_delete(target.instance_id, target.application_id, expired)
        .unwrap();
    assert!(repo.directory_restore_inputs(&target, &key).is_err());
    let retained = repo
        .directory_lease_inputs(target.instance_id, lease.identity, &key)
        .unwrap();
    assert_eq!(retained[0].1, Some(directory_image()));
    assert!(
        repo.directory_lease_inputs(target.instance_id, lease.identity, &"f".repeat(64))
            .is_err()
    );
    assert!(
        repo.directory_lease_inputs(InstanceId::generate(), lease.identity, &key)
            .is_err()
    );
    let artifact = (
        snapshot.artifact_sha256.unwrap(),
        snapshot.artifact_size.unwrap(),
    );
    assert!(
        WorkerRepository::new(storage.db())
            .referenced_artifacts()
            .unwrap()
            .contains(&artifact)
    );
    assert!(
        repo.finish_snapshot_retirement(target.instance_id, snapshot.id, true)
            .is_err()
    );
    let mut proof = ContainerTermination {
        opaque_engine_key: &key,
        start_identity: Some(&start),
        terminated: true,
        cleanup_complete: false,
        late_operations_fenced: true,
    };
    assert!(
        repo.finish_stopped(lease.identity, &proof, expired + 1)
            .is_err()
    );
    assert!(
        repo.finish_snapshot_retirement(target.instance_id, snapshot.id, true)
            .is_err()
    );
    proof.cleanup_complete = true;
    repo.finish_stopped(lease.identity, &proof, expired + 2)
        .unwrap();
    assert!(
        !WorkerRepository::new(storage.db())
            .referenced_artifacts()
            .unwrap()
            .contains(&artifact)
    );
    repo.finish_snapshot_retirement(target.instance_id, snapshot.id, true)
        .unwrap();
    // Historical plans do not become restore authority after their backup is retired.
    assert_eq!(
        repo.lease(target.instance_id, lease.identity)
            .unwrap()
            .directory_snapshots,
        spec.directory_snapshots
    );
}

#[test]
fn directory_restore_rejects_invalid_mixed_or_expired_plans_without_reserving_capacity() {
    let fixture = fixture(4);
    let source = source(&fixture);
    let directory = ready_directory(&fixture, &source);
    let whole = ready(&fixture, &source);
    let target = other_object(&fixture, 64);
    let key = "e".repeat(64);
    let repo = ContainerAuthority::new(fixture.storage.db());
    let mut spec = execution(&fixture.storage, &target, &fixture.spec, &key);
    for plan in [
        vec![mount(None, "/")],
        vec![mount(None, "/same"), mount(None, "/same")],
        vec![mount(Some(whole.id), "/data")],
        vec![
            mount(Some(directory.id), "/data"),
            mount(Some(ResourceId::generate()), "/missing"),
        ],
    ] {
        spec.directory_snapshots = plan;
        assert!(repo.reserve(&target, &key, &spec, 10, 44).is_err());
        assert!(repo.existing_lease(&target).unwrap().is_none());
        fixture
            .storage
            .db()
            .with_read(|conn| {
                let count: u64 = conn
                    .query_row(
                        "SELECT COALESCE(SUM(json_array_length(directory_restores_json)),0) FROM container_instance_leases",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(count, 0);
                Ok(())
            })
            .unwrap();
    }
    // A prior read cannot bypass expiration at the later reservation transaction.
    repo.directory_snapshot_for_restore(&target, directory.id, 44)
        .unwrap();
    spec.directory_snapshots = vec![mount(Some(directory.id), "/data")];
    assert!(
        repo.reserve(&target, &key, &spec, 10, directory.expires_at_ms)
            .is_err()
    );
    assert!(repo.existing_lease(&target).unwrap().is_none());
    for value in [
        serde_json::json!({"snapshotId":null,"mountPoint":"relative"}),
        serde_json::json!({"snapshotId":null,"mountPoint":"/data/../saved"}),
        serde_json::json!({"snapshotId":"not-a-resource-id","mountPoint":"/data"}),
        serde_json::json!({"snapshotId":null,"mountPoint":"/data","hostPath":"/operator"}),
    ] {
        assert!(serde_json::from_value::<ContainerDirectoryRestore>(value).is_err());
    }
}

#[test]
fn directory_restore_sql_identity_is_immutable_for_both_scheduling_policies() {
    let fixture = fixture(4);
    let source = source(&fixture);
    let snapshot = ready_directory(&fixture, &source);
    let target = other_object(&fixture, 65);
    let key = "f".repeat(64);
    let mut spec = execution(&fixture.storage, &target, &fixture.spec, &key);
    spec.directory_snapshots = vec![mount(Some(snapshot.id), "/data")];
    let repo = ContainerAuthority::new(fixture.storage.db());
    repo.reserve(&target, &key, &spec, 10, 44).unwrap();
    fixture.storage.db().with_immediate(|tx| {
        for replacement in [
            serde_json::json!([]),
            serde_json::json!([{"snapshotId":snapshot.id,"mountPoint":"/other"}]),
            serde_json::json!([
                {"snapshotId":snapshot.id,"mountPoint":"/data"},
                {"snapshotId":snapshot.id,"mountPoint":"/appended"},
            ]),
        ] {
            assert!(tx.execute("UPDATE container_instance_leases SET directory_restores_json=?2 WHERE opaque_engine_key=?1", params![&key,serde_json::to_vec(&replacement).unwrap()]).is_err());
        }
        Ok(())
    }).unwrap();
    let default = super::super::fixture(4);
    let source = super::source(&default);
    let snapshot = ready_directory(&default, &source);
    let target = other_object(&default, 66);
    let repo = ContainerAuthority::new(default.storage.db());
    let mut spec = execution(&default.storage, &target, &default.spec, &key);
    spec.directory_snapshots = vec![mount(Some(snapshot.id), "/data"), mount(None, "/empty")];
    let lease = repo.reserve(&target, &key, &spec, 10, 44).unwrap();
    assert_eq!(lease.directory_snapshots, spec.directory_snapshots);
    assert_eq!(
        repo.directory_restore_inputs(&target, &key).unwrap()[0].1,
        Some(directory_image())
    );
}
