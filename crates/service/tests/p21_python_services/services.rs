//! Real-daemon Service Binding parity using reviewed Python SDK bytes and ordinary JS peers.

use super::python_support::capture::Capture;
use super::python_support::fixture::{Fixture, RequestTarget};
use super::python_support::{PYTHON_SECRETS, platform_process};
use serde_json::{Value, json};
use std::fs;

const PYTHON: &str = "python-services-fixture";
const TARGET: &str = "python-services-js-target";
const PEER: &str = "python-services-js-peer";
const TARGET_SOURCE: &str = include_str!("target.js");
const PEER_SOURCE: &str = include_str!("peer.js");

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p21_python_services_fetch_named_rpc_callback_restart_rollback() {
    let capture = Capture::load(
        "test/fixtures/python-services",
        "test/applications/python-services/src",
        &[],
    )
    .await;
    let mut fixture = Fixture::new(None).await;
    let target_first = target_version(&fixture, "javascript-first").await;
    fixture.promote(TARGET, &target_first).await;
    let python_first = python_version(&fixture, &capture, "first", PYTHON_SECRETS[0]).await;
    let first_record = fixture.record(PYTHON, &python_first);
    fixture.promote(PYTHON, &python_first).await;
    let peer = fixture
        .upload_javascript(
            PEER,
            PEER_SOURCE,
            &[
                service_binding("TARGET", TARGET, None),
                service_binding("NAMED", TARGET, Some("NamedApi")),
                service_binding("PYTHON", PYTHON, None),
                service_binding("PY_NAMED", PYTHON, Some("NamedApi")),
            ],
            None,
            None,
        )
        .await;
    fixture.promote(PEER, &peer).await;
    verify(&fixture, "first", "javascript-first").await;
    let (status, _, _) = fixture
        .request(
            "/uncaught-failure",
            "GET",
            "application/json",
            Vec::new(),
            RequestTarget::Worker(PEER),
        )
        .await;
    assert_eq!(
        status, 500,
        "uncaught RPC errors must fail the public request"
    );

    fixture.process.stop().await;
    fixture.ciphertext(&first_record).await;
    fixture.mock.clear_recorded();
    fixture.process.restart(&fixture.config, &fixture.log);
    platform_process::ready(&fixture.client, fixture.admin, &mut fixture.process).await;
    verify(&fixture, "first", "javascript-first").await;
    assert_eq!(fixture.record(PYTHON, &python_first), first_record);
    assert_no_prepare_upload(&fixture);

    // A new invocation resolves the target's active deployment; changing the
    // target cannot reprepare or change the caller's immutable Python identity.
    let target_second = target_version(&fixture, "javascript-second").await;
    assert_eq!(fixture.active(TARGET), normalized(&target_first));
    fixture.promote(TARGET, &target_second).await;
    verify(&fixture, "first", "javascript-second").await;
    assert_eq!(fixture.record(PYTHON, &python_first), first_record);
    fixture.promote(TARGET, &target_first).await;
    fixture.restart().await;
    verify(&fixture, "first", "javascript-first").await;
    assert_eq!(fixture.record(PYTHON, &python_first), first_record);

    let python_second = python_version(&fixture, &capture, "second", PYTHON_SECRETS[1]).await;
    let second_record = fixture.record(PYTHON, &python_second);
    assert_ne!(
        first_record.prepared_identity_sha256,
        second_record.prepared_identity_sha256
    );
    assert_ne!(first_record.artifact_sha256, second_record.artifact_sha256);
    assert_eq!(fixture.active(PYTHON), normalized(&python_first));
    fixture.promote(PYTHON, &python_second).await;
    verify(&fixture, "second", "javascript-first").await;
    fixture.promote(PYTHON, &python_first).await;
    fixture.restart().await;
    verify(&fixture, "first", "javascript-first").await;
    assert_eq!(fixture.record(PYTHON, &python_first), first_record);
    assert_eq!(fixture.record(PYTHON, &python_second), second_record);

    // Persisted service descriptors may not turn a deleted target into a
    // successful response or retarget another resource after a fresh restart.
    let (status, _, body) = fixture
        .request(
            &format!(
                "/client/v4/accounts/{}/workers/scripts/{TARGET}",
                fixture.public_account
            ),
            "DELETE",
            "application/json",
            Vec::new(),
            RequestTarget::Admin,
        )
        .await;
    assert_eq!(
        status, 409,
        "referenced service target deletion must report a conflict"
    );
    let rejected: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(rejected["success"], false);
    assert_eq!(rejected["errors"][0]["code"], 9_100_006);
    assert_eq!(fixture.active(TARGET), normalized(&target_first));
    verify(&fixture, "first", "javascript-first").await;
    fixture
        .api(
            &format!("/workers/scripts/{TARGET}?force=true"),
            "DELETE",
            None,
        )
        .await;
    assert_missing_target(&fixture).await;
    fixture.restart().await;
    assert_missing_target(&fixture).await;
    assert_eq!(fixture.active(PYTHON), normalized(&python_first));
    assert_eq!(fixture.record(PYTHON, &python_first), first_record);
    assert_eq!(fixture.record(PYTHON, &python_second), second_record);
    // Calls into the Python deployment itself remain independent of the
    // missing outbound target and restore the original named/default class.
    assert_eq!(
        fixture.invoke(PEER, "/python-identify").await,
        json!({"entrypoint":"default", "revision":"first"})
    );
    assert_eq!(
        fixture.invoke(PEER, "/python-named-fetch").await,
        json!({"entrypoint":"named", "revision":"first"})
    );

    fixture.process.stop().await;
    for lease in [
        "child.lease",
        "python-prepare.lease",
        "python-compile.lease",
    ] {
        assert!(!fixture.data.join("runtime").join(lease).exists());
    }
    for address in [fixture.public, fixture.admin] {
        assert!(tokio::net::TcpListener::bind(address).await.is_ok());
    }
    let log = fs::read(&fixture.log).unwrap();
    for secret in PYTHON_SECRETS {
        assert!(
            !log.windows(secret.len())
                .any(|part| part == secret.as_bytes())
        );
    }
    println!(
        "python-services-evidence: {}",
        json!({
            "uploadSha256":capture.sha256,
            "pythonFirstVersion":python_first,"pythonSecondVersion":python_second,
            "javascriptFirstTargetVersion":target_first,"javascriptSecondTargetVersion":target_second,
            "javascriptPeerVersion":peer,
            "firstPreparedIdentitySha256":hex::encode(first_record.prepared_identity_sha256),
            "secondPreparedIdentitySha256":hex::encode(second_record.prepared_identity_sha256),
            "activePythonVersion":fixture.active(PYTHON),
            "deletedTargetRejectedBeforeAndAfterRestart":true,
        })
    );
}

fn normalized(version: &str) -> String {
    uuid::Uuid::parse_str(version).unwrap().to_string()
}

fn service_binding(name: &str, service: &str, entrypoint: Option<&str>) -> Value {
    let mut binding = json!({"name":name,"type":"service","service":service});
    if let Some(entrypoint) = entrypoint {
        binding["entrypoint"] = json!(entrypoint);
    }
    binding
}

async fn target_version(fixture: &Fixture, revision: &str) -> String {
    fixture
        .upload_javascript(
            TARGET,
            TARGET_SOURCE,
            &[
                json!({"name":"REVISION","type":"plain_text","text":revision}),
                json!({"name":"TOKEN","type":"secret_text","text":PYTHON_SECRETS[0]}),
            ],
            None,
            None,
        )
        .await
}

async fn python_version(
    fixture: &Fixture,
    capture: &Capture,
    revision: &str,
    secret: &str,
) -> String {
    let (status, _, body, version) = fixture
        .upload_version(
            PYTHON,
            &Capture::content_type(),
            capture.render(
                &[
                    json!({"name":"REVISION","type":"plain_text","text":revision}),
                    json!({"name":"TOKEN","type":"secret_text","text":secret}),
                    service_binding("TARGET", TARGET, None),
                    service_binding("NAMED", TARGET, Some("NamedApi")),
                ],
                None,
            ),
        )
        .await;
    assert_eq!(status, 200, "Python Service Version upload/prepare failed");
    let response: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(response["success"], true);
    version.unwrap().to_string()
}

fn structured_value(message: &str) -> Value {
    json!({"message":message,"nested":[null,true,{"integer":42,"float":1.25}]})
}

async fn verify(fixture: &Fixture, python_revision: &str, javascript_revision: &str) {
    for caller in [PYTHON, PEER] {
        assert_eq!(
            fixture.invoke(caller, "/fetch").await,
            json!({
                "method":"POST","body":"service-body","query":["one","two"],
                "header":"python","host":"service.example","revision":javascript_revision,
            })
        );
        assert_eq!(
            fixture.invoke(caller, "/named-fetch").await,
            json!({"entrypoint":"named","revision":javascript_revision})
        );
        assert_eq!(
            fixture.invoke(caller, "/rpc").await,
            json!({"value":structured_value("µ☁"),"revision":javascript_revision})
        );
        assert_eq!(
            fixture.invoke(caller, "/named-rpc").await,
            json!({"product":42})
        );
        assert_eq!(
            fixture.invoke(caller, "/callback").await,
            structured_value("µ☁!")
        );
        let failure = fixture.invoke(caller, "/failure").await;
        assert!(
            failure["error"]
                .as_str()
                .unwrap()
                .ends_with("python-services-business-failure")
        );
    }
    assert_eq!(
        fixture.invoke(PEER, "/python-identify").await,
        json!({"entrypoint":"default","revision":python_revision})
    );
    for path in ["/python-rpc", "/python-named-rpc"] {
        assert_eq!(
            fixture.invoke(PEER, path).await,
            json!({"value":structured_value("µ☁"),"revision":python_revision})
        );
    }
    assert_eq!(
        fixture.invoke(PEER, "/python-named-fetch").await,
        json!({"entrypoint":"named","revision":python_revision})
    );
}

async fn assert_missing_target(fixture: &Fixture) {
    for caller in [PYTHON, PEER] {
        for path in ["/fetch", "/named-fetch", "/rpc", "/named-rpc", "/callback"] {
            let (status, _, body) = fixture
                .request(
                    path,
                    "GET",
                    "application/json",
                    Vec::new(),
                    RequestTarget::Worker(caller),
                )
                .await;
            assert_eq!(status, 503, "deleted service target must fail closed");
            let response: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(response["error"]["code"], "SERVICE_TARGET_NOT_READY");
        }
    }
}

fn assert_no_prepare_upload(fixture: &Fixture) {
    assert!(fixture.mock.recorded().iter().all(|request| {
        !request.path.contains("/system/artifacts/v1/sha256/")
            || (request.method != "PUT" && request.method != "POST")
    }));
}
