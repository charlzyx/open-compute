//! One serial scenario owns cross-language message semantics and durable queue recovery.

use super::assertions::*;
use super::python_support::capture::Capture;
use super::python_support::fixture::Fixture;
use super::python_support::{PYTHON_SECRETS, platform_process};
use super::{DLQ, EVENTS, PEER, PYTHON};
use serde_json::{Value, json};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p21_python_queues_produce_consume_retry_dlq_restart_rollback() {
    let capture = Capture::load(
        "test/fixtures/python-queues",
        "test/applications/python-queues/src",
        &[],
    )
    .await;
    let mut fixture = Fixture::new(None).await;
    assert_read_only_creation_rejected(&fixture).await;
    let queue = create_queue(&fixture, EVENTS).await;
    let dlq = create_queue(&fixture, DLQ).await;
    let kv = fixture
        .api(
            "/storage/kv/namespaces",
            "POST",
            Some(json!({"title":"python-queue-observations"})),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let first = python_version(&fixture, &capture, &kv, "first", PYTHON_SECRETS[0]).await;
    let first_record = fixture.record(PYTHON, &first);
    fixture.promote(PYTHON, &first).await;
    let peer = fixture
        .upload_javascript(
            PEER,
            include_str!("peer.js"),
            &bindings(&kv, "javascript", PYTHON_SECRETS[0]),
            None,
            None,
        )
        .await;
    fixture.promote(PEER, &peer).await;
    let mut consumer = set_consumer(&fixture, &queue, None, PYTHON).await;

    send_pair(&fixture, "initial").await;
    assert_eq!(backlog(&fixture, &queue).await, 8);
    assert_no_observation(&fixture, "initial-python-json").await;
    for script in [PYTHON, PEER] {
        assert_eq!(
            fixture.invoke(script, "/invalid").await,
            json!({"rejected":[true,true,true]})
        );
    }
    assert_eq!(backlog(&fixture, &queue).await, 8);
    assert_persisted_frames(&fixture, 8);
    fixture.process.stop().await;
    fixture.ciphertext(&first_record).await;
    fixture.mock.clear_recorded();
    fixture.process.restart(&fixture.config, &fixture.log);
    platform_process::ready(&fixture.client, fixture.admin, &mut fixture.process).await;
    assert_eq!(fixture.record(PYTHON, &first), first_record);
    assert_no_prepare_upload(&fixture);
    assert_eq!(backlog(&fixture, &queue).await, 8);
    assert_no_observation(&fixture, "initial-python-json").await;
    pause(&fixture, &queue, false).await;
    verify_pair(&fixture, &queue, "initial", "first").await;

    assert_eq!(
        fixture.invoke(PYTHON, "/retry?phase=python-retry").await,
        json!({"sent":1})
    );
    let retry = wait_observation(&fixture, "python-retry", 2).await;
    assert_eq!(retry["attempts"], 2);
    assert!(retry.get("retry").is_none());
    wait_backlog(&fixture, &queue, 0).await;
    assert_eq!(
        fixture.invoke(PYTHON, "/fail?phase=python-dlq").await,
        json!({"sent":1})
    );
    wait_backlog(&fixture, &dlq, 1).await;
    wait_backlog(&fixture, &queue, 0).await;
    let failure = wait_observation(&fixture, "python-dlq", 2).await;
    assert_eq!(failure["attempts"], 2);
    assert_dlq_body(&fixture);
    fixture.restart().await;
    assert_eq!(backlog(&fixture, &dlq).await, 1);
    assert_eq!(lookup(&fixture, "python-dlq").await, failure);
    assert_eq!(fixture.record(PYTHON, &first), first_record);

    pause(&fixture, &queue, true).await;
    let second = python_version(&fixture, &capture, &kv, "second", PYTHON_SECRETS[1]).await;
    let second_record = fixture.record(PYTHON, &second);
    assert_ne!(
        first_record.prepared_identity_sha256,
        second_record.prepared_identity_sha256
    );
    assert_ne!(first_record.artifact_sha256, second_record.artifact_sha256);
    assert_eq!(fixture.active(PYTHON), normalized(&first));
    fixture.promote(PYTHON, &second).await;
    // Queue API attachments are managed separately from the immutable Worker
    // upload. Reapply the public consumer configuration after deployment,
    // as the application deploy tooling does; do not write private authority.
    let next_consumer = set_consumer(&fixture, &queue, None, PYTHON).await;
    assert_ne!(next_consumer, consumer);
    consumer = next_consumer;
    send_pair(&fixture, "second").await;
    fixture.restart().await;
    assert_eq!(backlog(&fixture, &queue).await, 8);
    pause(&fixture, &queue, false).await;
    verify_pair(&fixture, &queue, "second", "second").await;

    pause(&fixture, &queue, true).await;
    fixture.promote(PYTHON, &first).await;
    let next_consumer = set_consumer(&fixture, &queue, None, PYTHON).await;
    assert_ne!(next_consumer, consumer);
    consumer = next_consumer;
    send_pair(&fixture, "rollback").await;
    fixture.restart().await;
    pause(&fixture, &queue, false).await;
    verify_pair(&fixture, &queue, "rollback", "first").await;
    assert_eq!(fixture.record(PYTHON, &first), first_record);
    assert_eq!(fixture.record(PYTHON, &second), second_record);

    // Switching the real consumer proves both languages decode the same
    // persisted frames; restart must retain the new consumer generation.
    pause(&fixture, &queue, true).await;
    let updated = set_consumer(&fixture, &queue, Some(&consumer), PEER).await;
    assert_eq!(updated, consumer);
    send_pair(&fixture, "javascript").await;
    fixture.restart().await;
    assert_eq!(backlog(&fixture, &queue).await, 8);
    pause(&fixture, &queue, false).await;
    verify_pair(&fixture, &queue, "javascript", "javascript").await;

    // Immutable versions still reference the Queue. Public deletion must
    // reject that reference instead of orphaning a producer or rebinding it.
    assert_referenced_deletion_rejected(&fixture, &queue).await;
    send_pair(&fixture, "delete-refused").await;
    verify_pair(&fixture, &queue, "delete-refused", "javascript").await;
    fixture.restart().await;
    assert_referenced_deletion_rejected(&fixture, &queue).await;
    send_pair(&fixture, "delete-refused-restart").await;
    verify_pair(&fixture, &queue, "delete-refused-restart", "javascript").await;
    assert_eq!(backlog(&fixture, &queue).await, 0);
    assert_eq!(backlog(&fixture, &dlq).await, 1);
    assert_eq!(fixture.record(PYTHON, &first), first_record);
    assert_eq!(fixture.record(PYTHON, &second), second_record);
    assert_eq!(fixture.active(PYTHON), normalized(&first));
    fixture.process.stop().await;
    assert_clean_shutdown(&fixture).await;
    println!(
        "python-queues-evidence: {}",
        json!({
            "uploadSha256":capture.sha256,
            "firstVersion":first,"secondVersion":second,"peerVersion":peer,
            "firstIdentity":hex::encode(first_record.prepared_identity_sha256),
            "secondIdentity":hex::encode(second_record.prepared_identity_sha256),
            "queue":queue,"consumer":consumer,
            "dlq":dlq,"durableFrames":true,"crossLanguage":true,"restart":true,
            "rollback":true,"retryFirstSettlementWins":true,"referencedDeletionRejectedBeforeAndAfterRestart":true,
        })
    );
}

fn normalized(version: &str) -> String {
    uuid::Uuid::parse_str(version).unwrap().to_string()
}

pub(super) fn bindings(kv: &str, revision: &str, secret: &str) -> Vec<Value> {
    vec![
        json!({"name":"REVISION","type":"plain_text","text":revision}),
        json!({"name":"TOKEN","type":"secret_text","text":secret}),
        json!({"name":"EVENTS","type":"queue","queue_name":EVENTS}),
        json!({"name":"KV","type":"kv_namespace","namespace_id":kv}),
    ]
}

async fn python_version(
    fixture: &Fixture,
    capture: &Capture,
    kv: &str,
    revision: &str,
    secret: &str,
) -> String {
    let (status, _, bytes, version) = fixture
        .upload_version(
            PYTHON,
            &Capture::content_type(),
            capture.render(&bindings(kv, revision, secret), None),
        )
        .await;
    assert_eq!(status, 200, "Python Queue Version admission/prepare failed");
    let response: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(response["success"], true);
    version.unwrap().to_string()
}
