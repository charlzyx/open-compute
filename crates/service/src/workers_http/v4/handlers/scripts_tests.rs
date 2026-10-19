use super::*;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use open_compute_core::SecretString;
use open_compute_workers::{
    CanonicalBundle, CreateVersionRequest, ModuleInput, ModuleType, RuntimeValidator,
    ValidationCandidate, VersionBundle, VersionContent, VersionController, VersionRuntimeFeatures,
};
use serde_json::Value;
use std::sync::Arc;
use tower::ServiceExt as _;

#[tokio::test]
async fn script_upload_and_version_upload_project_distinct_authoritative_ids_and_replay() {
    let (_temp, _mock, state, account, storage) =
        crate::tests::initialized_worker_http_fixture().await;
    let api = state.worker_api().unwrap();
    let repository = WorkerRepository::new(storage.db());
    let worker = repository
        .create_worker(account, "upload-identity", RequestId::generate(), 1, 100)
        .unwrap()
        .0;
    let bundle = CanonicalBundle::build(
        "index.js",
        vec![ModuleInput {
            name: "index.js".to_owned(),
            module_type: ModuleType::EsModule,
            bytes: b"export default {}".to_vec(),
        }],
        api.bundle_limits,
    )
    .unwrap();
    // This unit isolates the HTTP projection from the separately maintained
    // real-workerd and original-cf deployment acceptance paths.
    let validator: Arc<dyn RuntimeValidator> = Arc::new(|_: ValidationCandidate| async { Ok(()) });
    let request = CreateVersionRequest {
        instance_id: account,
        worker_id: worker.id,
        idempotency_key: "script-upload-identity".to_owned(),
        content: VersionContent::Worker {
            bundle: VersionBundle::Bytes(bundle.into_bytes()),
            assets: None,
        },
        vars: BTreeMap::new(),
        secrets: BTreeMap::new(),
        bindings: BTreeMap::new(),
        services: BTreeMap::new(),
        runtime_features: VersionRuntimeFeatures {
            compatibility_date: "2026-09-08".to_owned(),
            ..Default::default()
        },
        queue_consumers: Vec::new(),
        crons: Vec::new(),
        deployment_source: Some(DeploymentSource::ScriptUpload),
        observability: None,
        request_id: RequestId::generate(),
        now_ms: 2,
    };
    let controller = VersionController::new(
        &storage,
        api.artifacts.clone(),
        validator,
        api.bundle_limits,
    );
    let CreateVersionOutcome::Applied(created) =
        controller.create_version(request.clone()).await.unwrap()
    else {
        panic!("expected a new Version");
    };
    let CreateVersionOutcome::Replay(replay) = controller.create_version(request).await.unwrap()
    else {
        panic!("expected an idempotent replay");
    };
    let replay: Value = serde_json::from_slice(&replay).unwrap();
    let replay_id: VersionId = replay["version"]["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(replay_id, created.version.id);
    let authority = crate::cloudflare_v4::accounts::V4InstanceContext::new(account, 1);
    let mut results = Vec::new();
    for id in [created.version.id, replay_id] {
        let snapshot = repository
            .version_snapshot(account, worker.id, id, false)
            .unwrap();
        let result = serde_json::to_value(
            UploadItem::from_snapshot(api, &authority, &snapshot, true).unwrap(),
        )
        .unwrap();
        assert_eq!(result["id"], worker.name);
        assert_eq!(result["deployment_id"], id.to_string());
        assert_eq!(
            result["etag"],
            hex::encode(created.version.worker_code_sha256)
        );
        assert!(result["created_on"].is_string());
        assert!(result["modified_on"].is_string());
        for field in ["number", "metadata", "annotations", "resources"] {
            assert!(
                result.get(field).is_none(),
                "PUT must not return Version field {field}"
            );
        }
        results.push(result);
    }
    assert_eq!(results[0], results[1]);
    let snapshot = repository
        .version_snapshot(account, worker.id, replay_id, false)
        .unwrap();
    assert_eq!(snapshot.worker.active_version_id, Some(replay_id));
    let deployment_id = snapshot.worker.active_deployment_id.unwrap();
    assert_ne!(deployment_id.to_string(), replay_id.to_string());
    assert_eq!(
        repository
            .get_deployment(account, worker.id, deployment_id)
            .unwrap()
            .version_id,
        replay_id
    );
    let version =
        serde_json::to_value(UploadItem::from_snapshot(api, &authority, &snapshot, false).unwrap())
            .unwrap();
    assert_eq!(version["id"], replay_id.to_string());
    assert!(version["number"].is_number());
    assert!(version["metadata"].is_object());
    assert!(version["resources"].is_object());
    assert!(version.get("deployment_id").is_none());
    let path = format!(
        "/client/v4/accounts/{}/workers/scripts/{}/versions/{replay_id}",
        authority.public_id(),
        worker.name,
    );
    let app = crate::http::admin_router(
        state
            .with_v4_tokens(
                SecretString::new("deployer-token"),
                SecretString::new("read-token"),
            )
            .with_v4_instance_context(authority),
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri(path)
                .header("authorization", "Bearer read-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let fetched: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(fetched["success"], true);
    assert_eq!(fetched["result"], version);
}
