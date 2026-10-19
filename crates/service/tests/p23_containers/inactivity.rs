//! Natural inactivity without an SDK monitor or forced Durable Object abort.

use super::*;
use std::path::Path;

async fn running(
    data: &Path,
    instance: InstanceId,
    application: ResourceId,
    object: &str,
) -> ContainerLease {
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            if let Some(lease) = leases(data, instance, application)
                .into_iter()
                .find(|lease| {
                    lease.identity.object_id.to_string() == object
                        && lease.state == ContainerLeaseState::Running
                })
            {
                return lease;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("raw inactivity main requires positive Running authority")
}

async fn stopped(
    data: &Path,
    instance: InstanceId,
    application: ResourceId,
    before: &ContainerLease,
    provider: &QualifiedContainerProvider,
) {
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            let lease = leases(data, instance, application)
                .into_iter()
                .find(|lease| lease.identity == before.identity)
                .unwrap();
            assert_eq!(lease.start_identity, before.start_identity);
            if lease.state == ContainerLeaseState::Stopped {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("inactivity requires positive persisted Stopped authority");
    let proof = provider.observe(&before.opaque_engine_key).await.unwrap();
    assert!(proof.terminated && proof.cleanup_complete && proof.late_operations_fenced);
    assert!(!proof.running);
}

pub(super) async fn natural_idle_stops_subject_and_preserves_control(
    client: &http::Client,
    public: &str,
    host: &str,
    data: &Path,
    instance: InstanceId,
    application: ResourceId,
    provider: &QualifiedContainerProvider,
) {
    assert!(leases(data, instance, application).is_empty());
    let subject = tenant(
        client,
        public,
        host,
        "/inactivity/start?object=idle-subject&duration=1000",
    )
    .await;
    assert_eq!(subject["ready"], "ready");
    assert_eq!(subject["running"], true);
    let subject_id = subject["objectId"].as_str().unwrap();
    let subject_lease = running(data, instance, application, subject_id).await;
    let control = tenant(
        client,
        public,
        host,
        "/inactivity/start?object=idle-control&duration=120000",
    )
    .await;
    assert_eq!(control["ready"], "ready");
    assert_eq!(control["running"], true);
    let control_id = control["objectId"].as_str().unwrap();
    assert_ne!(subject_id, control_id);
    let control_lease = running(data, instance, application, control_id).await;
    let control_before = provider
        .observe(&control_lease.opaque_engine_key)
        .await
        .unwrap();
    assert!(control_before.running && !control_before.late_operations_fenced);
    assert_eq!(
        Some(control_before.start_identity.as_str()),
        control_lease.start_identity.as_deref()
    );

    // No Worker request, monitor, explicit abort, or health poll occurs in this quiet window.
    let quiet = tokio::time::Instant::now();
    tokio::time::sleep(Duration::from_secs(90)).await;
    assert!(quiet.elapsed() >= Duration::from_secs(90));

    assert_eq!(
        tenant(
            client,
            public,
            host,
            "/inactivity/state?object=idle-subject"
        )
        .await,
        json!({"objectId":subject_id,"info":null,"running":false})
    );
    let state = tenant(
        client,
        public,
        host,
        "/inactivity/state?object=idle-control",
    )
    .await;
    assert_eq!(state["objectId"], control_id);
    assert_eq!(state["running"], true);
    assert!(state["info"].is_object());
    let current = running(data, instance, application, control_id).await;
    assert_eq!(current.identity, control_lease.identity);
    assert_eq!(current.start_identity, control_lease.start_identity);
    let control_after = provider.observe(&current.opaque_engine_key).await.unwrap();
    assert!(control_after.running && !control_after.late_operations_fenced);
    assert_eq!(control_after.start_identity, control_before.start_identity);
    stopped(data, instance, application, &subject_lease, provider).await;
    assert_eq!(
        tenant(client, public, host, "/inactivity/stop?object=idle-control").await,
        json!({"objectId":control_id,"info":null,"running":false})
    );
    stopped(data, instance, application, &control_lease, provider).await;
    let final_leases = leases(data, instance, application);
    assert_eq!(final_leases.len(), 2);
    assert!(
        final_leases
            .iter()
            .all(|lease| lease.state == ContainerLeaseState::Stopped)
    );
}
