use super::router;
use crate::containers::test_support::{Fixture, fixture};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::Request,
    http::{Method, StatusCode, header},
    response::Response,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use open_compute_core::InstanceId;
use open_compute_storage::containers::ContainerAuthority;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tower::ServiceExt as _;

fn app(fixture: &Fixture) -> Router {
    crate::cloudflare_v4::router(
        fixture.state.clone(),
        crate::cloudflare_v4::storage_router(),
    )
    .merge(router())
    .with_state(fixture.state.clone())
}

async fn request(app: &Router, method: Method, path: &str, auth: &str, bytes: &[u8]) -> Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header(header::AUTHORIZATION, auth)
                .header(header::HOST, "registry.example.test")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(bytes.to_vec()))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn container_registry_mounts_owned_blobs_falls_back_and_rejects_cross_instance_source() {
    let fixture = fixture().await;
    let app = app(&fixture);
    let instance = fixture.service.instance_id();
    let credentials = request(
        &app,
        Method::POST,
        &format!("/accounts/{instance}/containers/registries/registry.example.test/credentials"),
        "Bearer deployer-token",
        &serde_json::to_vec(&json!({"expiration_minutes":15,"permissions":["push","pull"]}))
            .unwrap(),
    )
    .await;
    assert_eq!(credentials.status(), StatusCode::OK);
    let credentials: Value =
        serde_json::from_slice(&to_bytes(credentials.into_body(), 1 << 20).await.unwrap()).unwrap();
    let auth = format!(
        "Basic {}",
        STANDARD.encode(format!(
            "{instance}:{}",
            credentials["result"]["password"].as_str().unwrap()
        ))
    );
    let source = format!("{instance}/source");
    let target = format!("{instance}/target");
    let bytes = b"verified cross-repository blob";
    let digest = format!("sha256:{}", hex::encode(Sha256::digest(bytes)));
    let start = request(
        &app,
        Method::POST,
        &format!("/v2/{source}/blobs/uploads/"),
        &auth,
        b"",
    )
    .await;
    assert_eq!(start.status(), StatusCode::ACCEPTED);
    let location = start.headers()[header::LOCATION].to_str().unwrap();
    let finish = request(
        &app,
        Method::PUT,
        &format!("{location}?digest={digest}"),
        &auth,
        bytes,
    )
    .await;
    assert_eq!(finish.status(), StatusCode::CREATED);
    let repo = ContainerAuthority::new(fixture.service.storage.db());
    let uploads_before = repo.registry_upload_ids(instance).unwrap();
    let mounted = request(
        &app,
        Method::POST,
        &format!("/v2/{target}/blobs/uploads/?mount={digest}&from={source}"),
        &auth,
        b"",
    )
    .await;
    assert_eq!(mounted.status(), StatusCode::CREATED);
    assert_eq!(mounted.headers()["docker-content-digest"], digest);
    assert_eq!(
        mounted.headers()[header::LOCATION],
        format!("/v2/{target}/blobs/{digest}")
    );
    assert_eq!(repo.registry_upload_ids(instance).unwrap(), uploads_before);
    let source_blob = repo.registry_blobs(instance, &source).unwrap().remove(0);
    let target_blob = repo.registry_blobs(instance, &target).unwrap().remove(0);
    assert_eq!(target_blob.sha256, source_blob.sha256);
    assert_eq!(target_blob.size, source_blob.size);
    let fetched = request(
        &app,
        Method::GET,
        &format!("/v2/{target}/blobs/{digest}"),
        &auth,
        b"",
    )
    .await;
    assert_eq!(fetched.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(fetched.into_body(), 1 << 20).await.unwrap(),
        bytes.as_slice()
    );
    let other_instance = InstanceId::generate();
    let denied = request(
        &app,
        Method::POST,
        &format!("/v2/{target}/blobs/uploads/?mount={digest}&from={other_instance}/source"),
        &auth,
        b"",
    )
    .await;
    assert_eq!(denied.status(), StatusCode::BAD_REQUEST);
    let denied: Value =
        serde_json::from_slice(&to_bytes(denied.into_body(), 1 << 20).await.unwrap()).unwrap();
    assert_eq!(denied["errors"][0]["code"], "DENIED");
    assert_eq!(repo.registry_upload_ids(instance).unwrap(), uploads_before);
    assert_eq!(repo.registry_blobs(instance, &target).unwrap().len(), 1);
    let missing_bytes = b"fallback upload has independent content";
    let missing_digest = format!("sha256:{}", hex::encode(Sha256::digest(missing_bytes)));
    let fallback = request(
        &app,
        Method::POST,
        &format!("/v2/{target}/blobs/uploads/?mount={missing_digest}&from={source}"),
        &auth,
        b"",
    )
    .await;
    assert_eq!(fallback.status(), StatusCode::ACCEPTED);
    assert!(!fallback.headers().contains_key("docker-content-digest"));
    let location = fallback.headers()[header::LOCATION].to_str().unwrap();
    assert!(location.starts_with(&format!("/v2/{target}/blobs/uploads/")));
    assert_eq!(
        repo.registry_upload_ids(instance).unwrap().len(),
        uploads_before.len() + 1
    );
    assert_eq!(repo.registry_blobs(instance, &target).unwrap().len(), 1);
    let finish = request(
        &app,
        Method::PUT,
        &format!("{location}?digest={missing_digest}"),
        &auth,
        missing_bytes,
    )
    .await;
    assert_eq!(finish.status(), StatusCode::CREATED);
    assert_eq!(finish.headers()["docker-content-digest"], missing_digest);
    assert_eq!(repo.registry_upload_ids(instance).unwrap(), uploads_before);
    assert_eq!(repo.registry_blobs(instance, &target).unwrap().len(), 2);
}
