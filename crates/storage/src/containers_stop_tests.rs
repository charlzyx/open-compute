//! Native stop completion uses one transactional facet and execution authority.

use super::*;

#[test]
fn authorized_stop_acknowledges_only_current_facet_stopped_intent_without_replacement() {
    let fixture = fixture(1);
    let repo = ContainerAuthority::new(fixture.storage.db());
    let instance = fixture.authority.instance_id;
    let reserve = |key: &str, now| {
        repo.reserve(
            &fixture.authority,
            key,
            &execution(&fixture.storage, &fixture.authority, &fixture.spec, key),
            10,
            now,
        )
        .unwrap()
    };
    // Positive termination alone does not invent an earlier authorized stop intent.
    let without_intent = reserve(&"a".repeat(64), 30);
    repo.finish_stopped(
        without_intent.identity,
        &termination(&without_intent.opaque_engine_key, None),
        31,
    )
    .unwrap();
    assert!(
        repo.begin_authorized_stop(
            &fixture.authority,
            without_intent.identity,
            &without_intent.opaque_engine_key,
            32
        )
        .is_err()
    );
    assert!(
        repo.lease(instance, without_intent.identity)
            .unwrap()
            .stop_requested_at_ms
            .is_none()
    );

    let stopped = reserve(&"b".repeat(64), 40);
    repo.begin_authorized_stop(
        &fixture.authority,
        stopped.identity,
        &stopped.opaque_engine_key,
        41,
    )
    .unwrap();
    repo.finish_stopped(
        stopped.identity,
        &termination(&stopped.opaque_engine_key, None),
        42,
    )
    .unwrap();
    let stopped = repo.lease(instance, stopped.identity).unwrap();
    repo.begin_authorized_stop(
        &fixture.authority,
        stopped.identity,
        &stopped.opaque_engine_key,
        43,
    )
    .unwrap();
    assert!(repo.lease(instance, stopped.identity).unwrap() == stopped);

    for mutation in 0..5 {
        let mut authority = fixture.authority;
        let mut identity = stopped.identity;
        let mut key = stopped.opaque_engine_key.clone();
        match mutation {
            0 => authority.route_generation += 1,
            1 => identity.namespace_resource_id = ResourceId::generate(),
            2 => identity.object_generation += 1,
            3 => key = "f".repeat(64),
            _ => {
                authority.object_id =
                    insert_object(&fixture.storage, authority.namespace_resource_id, 8);
            }
        }
        assert!(
            repo.begin_authorized_stop(&authority, identity, &key, 44)
                .is_err()
        );
        assert!(repo.lease(instance, stopped.identity).unwrap() == stopped);
    }
    let replacement = reserve(&"c".repeat(64), 50);
    assert!(
        repo.begin_authorized_stop(
            &fixture.authority,
            stopped.identity,
            &stopped.opaque_engine_key,
            51
        )
        .is_err()
    );
    assert!(repo.lease(instance, replacement.identity).unwrap() == replacement);
    assert!(repo.lease(instance, stopped.identity).unwrap() == stopped);
    assert_eq!(
        repo.existing_lease(&fixture.authority)
            .unwrap()
            .unwrap()
            .opaque_engine_key,
        replacement.opaque_engine_key
    );
}
