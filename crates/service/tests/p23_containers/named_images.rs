//! Named image authority through the original cf producer and native attachment.

use super::*;
use std::path::Path;

pub(super) fn application(data: &Path, instance: InstanceId) -> ResourceId {
    let db = ControlDb::open_readonly_wal_aware(&data.join("control.sqlite"), 1000).unwrap();
    let application = ContainerAuthority::new(&db)
        .applications(instance)
        .unwrap()
        .into_iter()
        .find(|app| app.name == "containers-named-fixture")
        .unwrap();
    assert_eq!(
        application.scheduling_policy,
        open_compute_storage::containers::ContainerSchedulingPolicy::DurableObject
    );
    application.id
}

pub(super) async fn assert_images(
    client: &http::Client,
    public: &str,
    host: &str,
    images: [&str; 2],
) {
    assert_eq!(
        tenant(client, public, host, "/named/images?object=named-images").await,
        json!({"images":{"base":images[0],"alternate":images[1]},"independent":true})
    );
}

pub(super) async fn assert_stopped(
    data: &Path,
    instance: InstanceId,
    application: ResourceId,
    provider: &QualifiedContainerProvider,
) {
    for lease in leases(data, instance, application) {
        assert_eq!(lease.state, ContainerLeaseState::Stopped);
        let proof = provider.observe(&lease.opaque_engine_key).await.unwrap();
        assert!(proof.terminated && proof.cleanup_complete && proof.late_operations_fenced);
        assert!(!proof.running);
    }
}

pub(super) async fn lifecycle(
    client: &http::Client,
    public: &str,
    host: &str,
    data: &Path,
    scope: (InstanceId, ResourceId),
    provider: &QualifiedContainerProvider,
    images: [&str; 2],
) {
    let (instance, application) = scope;
    assert!(leases(data, instance, application).is_empty());
    assert_images(client, public, host, images).await;
    assert_eq!(
        tenant(client, public, host, "/named/missing?object=named-missing").await,
        json!({"denied":true,"running":false,"info":null})
    );
    assert!(leases(data, instance, application).is_empty());
    provider.ensure_active().await.unwrap();
    let mut previous = None;
    for (alias, reference, output, instance_query) in [
        ("base", images[0], "one", ""),
        ("alternate", images[1], "two", ""),
        ("base", images[0], "one", "&instance=lite"),
    ] {
        let value = tenant(
            client,
            public,
            host,
            &format!("/named/start?object=named-start&alias={alias}{instance_query}"),
        )
        .await;
        assert_eq!(value["image"], output);
        assert_eq!(value["running"], true);
        assert_eq!(
            value["info"],
            json!({"image":reference,"labels":{"purpose":"containers-fixture"}})
        );
        let lease = wait_running(data, instance, application, 1).await;
        assert_eq!(lease.startup.image_reference, reference);
        let digest: [u8; 32] = hex::decode(reference.rsplit_once("@sha256:").unwrap().1)
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(lease.effective_image_sha256, digest);
        let lite = open_compute_core::containers::ContainerInstanceProfile::Lite
            .resources(provider.resources())
            .unwrap();
        let mut expected_resources = lite;
        if alias == "alternate" {
            expected_resources.cpu_nanos = 31_250_000;
            expected_resources.memory_bytes = 128 << 20;
            expected_resources.disk_bytes = 1_000_000_000;
            assert_ne!(expected_resources.cpu_nanos, lite.cpu_nanos);
            assert_ne!(expected_resources.memory_bytes, lite.memory_bytes);
            assert_ne!(expected_resources.disk_bytes, lite.disk_bytes);
        }
        assert_eq!(lease.effective_resources, expected_resources);
        if let Some(identity) = previous {
            assert_ne!(lease.identity, identity);
        }
        previous = Some(lease.identity);
        let proof = provider.observe(&lease.opaque_engine_key).await.unwrap();
        assert!(proof.running && !proof.terminated);
        assert_eq!(
            lease.start_identity.as_deref(),
            Some(proof.start_identity.as_str())
        );
        assert_eq!(
            tenant(client, public, host, "/named/stop?object=named-start").await,
            json!({"running":false})
        );
        assert_eq!(
            tenant(client, public, host, "/named/inspection?object=named-start").await,
            json!({"running":false,"info":null,"images":{"base":images[0],"alternate":images[1]}})
        );
        assert_stopped(data, instance, application, provider).await;
    }
}
