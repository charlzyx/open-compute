//! One cohesive ordinary Python deployment, persistence, restart and recovery scenario.

use super::*;
use bindings::{assert_peer_state, verify_shared_bindings};
use capture::{Bindings, Capture, UploadFault};
use fixture::{Fixture, RequestTarget};
use hmac::{Hmac, Mac as _};
use open_compute_artifacts::ArtifactRef;
use serde_json::{Value, json};
use sha2::Sha256;
use std::fs;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p21_python_main_upload_prepare_dispatch_restart_rollback() {
    // Qualify immutable input before starting a daemon. Missing builder output
    // fails, rather than selecting an old CLI or constructing replacement SDK.
    let capture = Capture::load(
        "test/fixtures/python-main",
        "test/applications/python-main/src",
        &["greeting/__init__.py", "greeting/message.json"],
    )
    .await;
    let mut fixture = Fixture::new(None).await;
    let outbound_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let outbound_url = format!("http://{}/", outbound_listener.local_addr().unwrap());
    let (stop_outbound, stopped_outbound) = tokio::sync::oneshot::channel();
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(async move {
        axum::serve(
            outbound_listener,
            axum::Router::new().route("/", axum::routing::get(|| async { "python-host-network" })),
        )
        .with_graceful_shutdown(async {
            let _ = stopped_outbound.await;
        })
        .await
        .unwrap();
    });
    let kv = fixture
        .api(
            "/storage/kv/namespaces",
            "POST",
            Some(json!({"title":"python-main-kv"})),
        )
        .await;
    let d1 = fixture
        .api(
            "/d1/database",
            "POST",
            Some(json!({"name":"python-main-db"})),
        )
        .await;
    let bucket = "python-main-bucket";
    fixture
        .api("/r2/buckets", "POST", Some(json!({"name":bucket})))
        .await;
    let kv = kv["id"].as_str().unwrap();
    let d1 = d1["uuid"].as_str().unwrap();
    let first_bindings = Bindings {
        kv,
        d1,
        r2: bucket,
        outbound: &outbound_url,
        revision: "first",
        secret: PYTHON_SECRETS[0],
    };
    let first_body = capture.render(&first_bindings.metadata(), None);
    authorization::assert_upload_authorization(&fixture, &first_body).await;
    for fault in [
        UploadFault::WrongMainMime,
        UploadFault::MissingMain,
        UploadFault::UnknownFlag,
        UploadFault::DuplicateModule,
        UploadFault::ReservedModule,
    ] {
        let (status, _, body, version) = fixture
            .upload_version(
                SCRIPT,
                &Capture::content_type(),
                capture.render(&first_bindings.metadata(), Some(fault)),
            )
            .await;
        assert_eq!(status, 400, "{fault:?} must fail admission");
        assert!(version.is_none());
        let response: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(response["success"], false);
        let prepared: i64 = fixture
            .connection()
            .query_row("SELECT count(*) FROM version_python_prepared", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            prepared, 0,
            "{fault:?} must never publish a prepared snapshot"
        );
    }
    let first = upload(&fixture, first_body).await;
    let first_record = fixture.record(SCRIPT, &first);
    fixture.promote(SCRIPT, &first).await;
    assert_eq!(fixture.active(SCRIPT), first_record.version_id.to_string());
    assert_secret(&fixture, PYTHON_SECRETS[0]).await;
    assert_eq!(
        fixture.invoke(SCRIPT, "/").await,
        json!({"revision":"first", "package":"immutable-package-data"})
    );
    assert_eq!(
        fixture.invoke(SCRIPT, "/write").await,
        json!({"written":"first", "package":"immutable-package-data"})
    );
    assert_state(&fixture, "first", "first").await;
    let peer_version = verify_shared_bindings(&fixture, &first_bindings).await;
    assert_state(&fixture, "first", "first").await;
    let (status, _, stream) = fixture
        .request(
            "/stream",
            "GET",
            "application/json",
            Vec::new(),
            RequestTarget::Worker(SCRIPT),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(stream, b"first-second");
    let (status, _, response) = fixture
        .request(
            "/outbound",
            "GET",
            "application/json",
            Vec::new(),
            RequestTarget::Worker(SCRIPT),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(response, b"python-host-network");
    let (status, _, _) = fixture
        .request(
            "/exception",
            "GET",
            "application/json",
            Vec::new(),
            RequestTarget::Worker(SCRIPT),
        )
        .await;
    assert_eq!(status, 500);
    assert_state(&fixture, "first", "first").await;
    let settings = fixture
        .api(&format!("/workers/scripts/{SCRIPT}/settings"), "GET", None)
        .await;
    assert!(
        settings["bindings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|binding| binding["name"] == "TOKEN"
                && binding["type"] == "secret_text"
                && binding.get("text").is_none())
    );

    // Reap both daemon and workerd; inspect actual ciphertext/AEAD under the
    // same retained master key, then boot a fresh runtime generation.
    fixture.process.stop().await;
    let ciphertext = fixture.ciphertext(&first_record).await;
    fixture.mock.clear_recorded();
    fixture.process.restart(&fixture.config, &fixture.log);
    platform_process::ready(&fixture.client, fixture.admin, &mut fixture.process).await;
    assert_state(&fixture, "first", "first").await;
    assert_peer_state(&fixture, Some("first")).await;
    assert_secret(&fixture, PYTHON_SECRETS[0]).await;
    assert_eq!(fixture.record(SCRIPT, &first), first_record);
    assert_no_prepare_upload(&fixture);
    authorization::assert_upload_authorization(
        &fixture,
        &capture.render(&first_bindings.metadata(), None),
    )
    .await;
    assert_eq!(fixture.active(SCRIPT), first_record.version_id.to_string());
    assert_state(&fixture, "first", "first").await;

    bindings::assert_bound_resources_retained(&fixture, &first_bindings).await;
    assert_state(&fixture, "first", "first").await;
    assert_peer_state(&fixture, Some("first")).await;

    let second_body = capture.render(
        &Bindings {
            revision: "second",
            secret: PYTHON_SECRETS[1],
            ..first_bindings
        }
        .metadata(),
        None,
    );
    let second = upload(&fixture, second_body).await;
    let second_record = fixture.record(SCRIPT, &second);
    assert_ne!(
        first_record.prepared_identity_sha256,
        second_record.prepared_identity_sha256
    );
    assert_ne!(first_record.artifact_sha256, second_record.artifact_sha256);
    assert_eq!(
        fixture.active(SCRIPT),
        first_record.version_id.to_string(),
        "upload cannot promote implicitly"
    );
    fixture.promote(SCRIPT, &second).await;
    assert_state(&fixture, "second", "first").await;
    assert_secret(&fixture, PYTHON_SECRETS[1]).await;
    assert_eq!(fixture.invoke(SCRIPT, "/write").await["written"], "second");
    assert_state(&fixture, "second", "second").await;
    assert_peer_state(&fixture, Some("second")).await;
    fixture.promote(SCRIPT, &first).await;
    assert_state(&fixture, "first", "second").await;
    assert_secret(&fixture, PYTHON_SECRETS[0]).await;
    assert_peer_state(&fixture, Some("second")).await;
    assert_eq!(fixture.record(SCRIPT, &first), first_record);
    fixture.restart().await;
    assert_state(&fixture, "first", "second").await;
    assert_secret(&fixture, PYTHON_SECRETS[0]).await;
    assert_peer_state(&fixture, Some("second")).await;
    assert_eq!(fixture.record(SCRIPT, &first), first_record);
    assert_eq!(fixture.record(SCRIPT, &second), second_record);

    // Syntax, missing package/data and top-level exceptions fail preparation
    // without changing deployment authority. All SDK modules remain unchanged.
    let rejected_before: i64 = fixture.connection().query_row(
        "SELECT count(*) FROM worker_versions WHERE state='rejected' AND rejection_code='BUNDLE_RUNTIME_INVALID'",
        [], |row| row.get(0)).unwrap();
    let faults = [
        UploadFault::MainSyntax,
        UploadFault::PackageSyntax,
        UploadFault::ImportException,
        UploadFault::PackageMissing,
        UploadFault::PackageDataMissing,
        UploadFault::PackageDataCorrupt,
    ];
    for fault in faults {
        let (status, _, body, version) = fixture
            .upload_version(
                SCRIPT,
                &Capture::content_type(),
                capture.render(&first_bindings.metadata(), Some(fault)),
            )
            .await;
        assert_eq!(status, 400, "{fault:?} must fail Python prepare");
        assert!(version.is_none());
        let rejection: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(rejection["success"], false);
        assert_eq!(rejection["errors"][0]["code"], 10021);
        assert_eq!(fixture.active(SCRIPT), first_record.version_id.to_string());
        assert_eq!(fixture.record(SCRIPT, &first), first_record);
        assert_eq!(fixture.record(SCRIPT, &second), second_record);
        for lease in ["python-prepare.lease", "python-compile.lease"] {
            assert!(!fixture.data.join("runtime").join(lease).exists());
        }
        assert_state(&fixture, "first", "second").await;
        assert_peer_state(&fixture, Some("second")).await;
    }
    let connection = fixture.connection();
    let invalid_versions: i64 = connection.query_row(
        "SELECT count(*) FROM worker_versions WHERE state='rejected' AND rejection_code='BUNDLE_RUNTIME_INVALID'",
        [], |row| row.get(0)).unwrap();
    assert_eq!(
        invalid_versions,
        rejected_before + i64::try_from(faults.len()).unwrap()
    );
    let prepared_rejections: i64 = connection.query_row(
        "SELECT count(*) FROM version_python_prepared p JOIN worker_versions v ON v.id=p.version_id WHERE v.state='rejected'",
        [], |row| row.get(0)).unwrap();
    assert_eq!(prepared_rejections, 0);
    drop(connection);
    assert_eq!(fixture.active(SCRIPT), first_record.version_id.to_string());
    assert_state(&fixture, "first", "second").await;

    let reference = ArtifactRef::new(
        1,
        &hex::encode(first_record.artifact_sha256),
        first_record.artifact_size,
    )
    .unwrap();
    let key = reference.physical_key("system/");
    // Native memory caches disappear on each restart. Cover both size mismatch
    // and same-size digest mismatch without repeating equivalent length faults.
    for partial in [true, false] {
        fixture.process.stop().await;
        if partial {
            fixture
                .mock
                .put_raw(&key, ciphertext[..ciphertext.len() - 1].to_vec());
        } else {
            let mut changed = ciphertext.clone();
            *changed.last_mut().unwrap() ^= 1;
            assert_eq!(changed.len(), ciphertext.len());
            fixture.mock.put_raw(&key, changed);
        }
        fixture.mock.clear_recorded();
        fixture.process.restart(&fixture.config, &fixture.log);
        platform_process::ready(&fixture.client, fixture.admin, &mut fixture.process).await;
        let (status, _, _) = fixture
            .request(
                "/read",
                "GET",
                "application/json",
                Vec::new(),
                RequestTarget::Worker(SCRIPT),
            )
            .await;
        assert_eq!(status, 500);
        assert!(
            fixture
                .mock
                .recorded()
                .iter()
                .any(|request| request.method == "GET" && request.path.ends_with(&key)),
            "restore must read the deliberately damaged prepared object"
        );
        assert_peer_state(&fixture, Some("second")).await;
        assert_eq!(fixture.active(SCRIPT), first_record.version_id.to_string());
        assert_eq!(fixture.record(SCRIPT, &first), first_record);
        assert_eq!(fixture.record(SCRIPT, &second), second_record);
        for lease in ["python-prepare.lease", "python-compile.lease"] {
            assert!(!fixture.data.join("runtime").join(lease).exists());
        }
        assert_no_prepare_upload(&fixture);
        fixture.process.stop().await;
        fixture.mock.put_raw(&key, ciphertext.clone());
        fixture.process.restart(&fixture.config, &fixture.log);
        platform_process::ready(&fixture.client, fixture.admin, &mut fixture.process).await;
        assert_state(&fixture, "first", "second").await;
        assert_peer_state(&fixture, Some("second")).await;
        assert_secret(&fixture, PYTHON_SECRETS[0]).await;
    }
    assert_eq!(
        fixture.invoke(SCRIPT, "/delete").await,
        json!({"deleted":true})
    );
    let empty = fixture.invoke(SCRIPT, "/read").await;
    assert_peer_state(&fixture, None).await;
    assert_eq!(
        empty,
        json!({"kv":null,"d1":null,"r2":null,"revision":"first"})
    );
    fixture.process.stop().await;
    for lease in [
        "child.lease",
        "python-prepare.lease",
        "python-compile.lease",
    ] {
        assert!(
            !fixture.data.join("runtime").join(lease).exists(),
            "normal shutdown retained {lease}"
        );
    }
    for address in [fixture.public, fixture.admin] {
        assert!(
            tokio::net::TcpListener::bind(address).await.is_ok(),
            "daemon retained a listener"
        );
    }
    let logs = fs::read(&fixture.log).unwrap();
    for secret in PYTHON_SECRETS {
        assert!(
            !logs
                .windows(secret.len())
                .any(|part| part == secret.as_bytes()),
            "daemon log exposed Python secret"
        );
    }
    stop_outbound.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), tasks.join_next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let evidence = json!({
        "case":"python_main::p21_python_main_upload_prepare_dispatch_restart_rollback",
        "uploadSha256":capture.sha256,"firstVersion":first,"secondVersion":second,
        "javascriptPeerVersion":peer_version,"sharedBindings":["KV","D1","R2"],
        "firstPreparedIdentitySha256":hex::encode(first_record.prepared_identity_sha256),
        "secondPreparedIdentitySha256":hex::encode(second_record.prepared_identity_sha256),
        "firstPreparedArtifactSha256":hex::encode(first_record.artifact_sha256),
        "secondPreparedArtifactSha256":hex::encode(second_record.artifact_sha256),
        "activeVersion":fixture.active(SCRIPT),"resourceState":"deleted by original Python deployment",
        "packagePreparationRejectionCases":faults.len(),"importExceptionSecretRedacted":true,
        "preparedCorruptionCases":["truncated size mismatch","same-size digest mismatch"],
        "corruptedObjectFetched":true,"preparedRecordsUnchangedOnCorruption":true,
        "secretProofAfterPromotionRestartRollbackAndRecovery":true
    });
    fs::write(
        fixture.root.join("evidence.json"),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
    // The successful private scope is removed by its ownership guard. Keep the
    // sanitized authority evidence in the Gate runner's retained stdout log.
    println!("python-main-evidence: {evidence}");
}

async fn upload(fixture: &Fixture, bytes: Vec<u8>) -> String {
    let (status, _, bytes, version) = fixture
        .upload_version(SCRIPT, &Capture::content_type(), bytes)
        .await;
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        status, 200,
        "ordinary Python Version upload/prepare failed: {value}"
    );
    assert_eq!(value["success"], true);
    version.unwrap().to_string()
}

async fn assert_state(fixture: &Fixture, revision: &str, stored: &str) {
    assert_eq!(
        fixture.invoke(SCRIPT, "/read").await,
        json!({"kv":stored,"d1":stored,"r2":stored,"revision":revision})
    );
}

async fn assert_secret(fixture: &Fixture, secret: &str) {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(b"python-main-secret-proof");
    assert_eq!(
        fixture.invoke(SCRIPT, "/secret-proof").await,
        json!({"proof":hex::encode(mac.finalize().into_bytes())})
    );
}

fn assert_no_prepare_upload(fixture: &Fixture) {
    assert!(
        fixture.mock.recorded().iter().all(|request| {
            !request.path.contains("/system/artifacts/v1/sha256/")
                || (request.method != "PUT" && request.method != "POST")
        }),
        "retained restore must not upload a new prepared/source object"
    );
}
