use crate::containers::test_support::{Fixture, authority, fixture};
use crate::containers::wire::{Configuration, Disk};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use open_compute_core::{InstanceId, ResourceId};
use open_compute_storage::containers::{ContainerApplicationMetadataPatch, ContainerAuthority};
use serde_json::Value;
use tower::ServiceExt as _;

fn router(fixture: &Fixture) -> Router {
    crate::cloudflare_v4::router(
        fixture.state.clone(),
        crate::cloudflare_v4::storage_router(),
    )
    .with_state(fixture.state.clone())
}

async fn get(router: &Router, path: &str, authorization: &str) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header(header::AUTHORIZATION, authorization)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap();
    (status, value)
}

#[tokio::test]
async fn container_rollouts_require_live_application_and_preserve_internal_history() {
    let fixture = fixture().await;
    let image = fixture.admit_image("rollout-public-authority").await;
    let authority = authority(&fixture.service, &fixture._artifacts, image, true);
    let instance = fixture.service.instance_id();
    let repo = ContainerAuthority::new(fixture.service.storage.db());
    let spec = repo.target(instance, authority.app.id, 1).unwrap().spec;
    let configuration = Configuration {
        image: format!(
            "registry.example.test/{instance}/authority-app@sha256:{}",
            hex::encode(image)
        ),
        instance_type: None,
        vcpu: Some(spec.resources.cpu_nanos as f64 / 1e9),
        memory_mib: Some(spec.resources.memory_bytes / (1 << 20)),
        disk: Some(Disk {
            size_mb: spec.resources.disk_bytes as f64 / 1e6,
        }),
    };
    assert_eq!(
        configuration.resources(spec.resources).unwrap(),
        spec.resources
    );
    repo.commit_application_configuration(
        instance,
        &authority.app,
        &spec,
        &serde_json::to_vec(&configuration).unwrap(),
        ContainerApplicationMetadataPatch {
            max_instances: Some(4),
            observability_logs: Some(false),
        },
        9,
    )
    .unwrap();
    let first = repo
        .create_rollout(
            instance,
            authority.app.id,
            &open_compute_storage::containers::ContainerRolloutCreate {
                id: ResourceId::generate(),
                target_configuration: &serde_json::to_vec(&configuration).unwrap(),
                steps: &[50, 100],
                details: None,
            },
            10,
        )
        .unwrap();
    for now in [11, 12] {
        assert!(
            repo.rollout_step_leases(instance, authority.app.id, first.id, now)
                .unwrap()
                .is_empty()
        );
    }
    let second = repo
        .create_rollout(
            instance,
            authority.app.id,
            &open_compute_storage::containers::ContainerRolloutCreate {
                id: ResourceId::generate(),
                target_configuration: &serde_json::to_vec(&configuration).unwrap(),
                steps: &[100],
                details: None,
            },
            20,
        )
        .unwrap();
    let application = router(&fixture);
    let base = format!(
        "/accounts/{instance}/containers/applications/{}",
        authority.app.id
    );
    let list = format!("{base}/rollouts");
    let first_path = format!("{list}/{}", first.id);
    let second_path = format!("{list}/{}", second.id);
    let (status, all) = get(&application, &list, "Bearer read-token").await;
    assert_eq!(status, StatusCode::OK, "{all}");
    assert_eq!(all["result"].as_array().unwrap().len(), 2);
    assert_eq!(all["result"][0]["id"], first.id.to_string());
    assert_eq!(all["result"][1]["id"], second.id.to_string());
    for (path, id, state) in [
        (&first_path, first.id, "completed"),
        (&second_path, second.id, "progressing"),
    ] {
        let (status, result) = get(&application, path, "Bearer read-token").await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["result"]["id"], id.to_string());
        assert_eq!(result["result"]["status"], state);
        assert_eq!(
            result["result"]["target_configuration"]["image"],
            configuration.image
        );
        assert_eq!(result["result"]["progress"]["total_instances"], 0);
    }
    let (status, page) = get(
        &application,
        &format!("{list}?per_page=1"),
        "Bearer read-token",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["result"].as_array().unwrap().len(), 1);
    assert_eq!(page["result"][0]["id"], first.id.to_string());
    let cursor = page["result_info"]["next_page_token"].as_str().unwrap();
    assert!(!cursor.is_empty());
    let (status, page) = get(
        &application,
        &format!("{list}?per_page=1&page_token={cursor}"),
        "Bearer read-token",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["result"].as_array().unwrap().len(), 1);
    assert_eq!(page["result"][0]["id"], second.id.to_string());
    assert_eq!(page["result_info"]["next_page_token"], "");
    for query in [
        "unknown=1",
        "per_page=0",
        "per_page=1001",
        "per_page=invalid",
        "page_token=!",
        "page_token=OTk",
        "per_page=1&per_page=2",
    ] {
        let (status, error) = get(
            &application,
            &format!("{list}?{query}"),
            "Bearer read-token",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {error}");
        assert_eq!(error["success"], false);
    }
    assert_eq!(
        get(
            &application,
            &format!("{first_path}?per_page=1"),
            "Bearer read-token"
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    for path in [&list, &first_path] {
        assert_eq!(
            get(&application, path, "Bearer invalid-token").await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    let missing = ResourceId::generate();
    let other_instance = InstanceId::generate();
    for path in [
        format!("{list}/{}", ResourceId::generate()),
        format!("/accounts/{instance}/containers/applications/{missing}/rollouts"),
        format!(
            "/accounts/{instance}/containers/applications/{missing}/rollouts/{}",
            first.id
        ),
        format!(
            "/accounts/{other_instance}/containers/applications/{}/rollouts",
            authority.app.id
        ),
        format!(
            "/accounts/{other_instance}/containers/applications/{}/rollouts/{}",
            authority.app.id, first.id
        ),
    ] {
        assert_eq!(
            get(&application, &path, "Bearer read-token").await.0,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    repo.begin_application_delete(instance, authority.app.id, 30)
        .unwrap();
    repo.finish_application_delete(instance, authority.app.id, 31)
        .unwrap();
    for path in [&list, &first_path, &second_path] {
        assert_eq!(
            get(&application, path, "Bearer read-token").await.0,
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(repo.rollouts(instance, authority.app.id).unwrap().len(), 2);
    assert_eq!(
        repo.rollout(instance, authority.app.id, first.id)
            .unwrap()
            .state,
        "completed"
    );
    assert_eq!(
        repo.rollout(instance, authority.app.id, second.id)
            .unwrap()
            .state,
        "running"
    );
}

#[tokio::test]
async fn container_rollout_post_conflicts_while_active_and_accepts_after_completion() {
    let fixture = fixture().await;
    let image = fixture.admit_image("rollout-post-conflict").await;
    let authority = authority(&fixture.service, &fixture._artifacts, image, true);
    let instance = fixture.service.instance_id();
    let repo = ContainerAuthority::new(fixture.service.storage.db());
    let (configuration, _, _) = fixture
        .service
        .application_configuration(&authority.app)
        .unwrap();
    let bytes = serde_json::to_vec(&serde_json::json!({
        "strategy":"rolling",
        "kind":"full_auto",
        "target_configuration":configuration,
        "steps":[{"step_size":{"percentage":100}}],
    }))
    .unwrap();
    let application = router(&fixture);
    let path = format!(
        "/accounts/{instance}/containers/applications/{}/rollouts",
        authority.app.id
    );
    let mut first = None;
    for (attempt, expected) in [StatusCode::OK, StatusCode::CONFLICT, StatusCode::OK]
        .into_iter()
        .enumerate()
    {
        if attempt == 2 {
            let id = first.unwrap();
            let now_ms = repo
                .rollout(instance, authority.app.id, id)
                .unwrap()
                .created_at_ms
                + 1;
            assert!(
                repo.rollout_step_leases(instance, authority.app.id, id, now_ms)
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                repo.rollout(instance, authority.app.id, id).unwrap().state,
                "completed"
            );
        }
        let response = application
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&path)
                    .header(header::AUTHORIZATION, "Bearer deployer-token")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(bytes.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let value: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap())
                .unwrap();
        assert_eq!(status, expected, "{value}");
        let rollouts = repo.rollouts(instance, authority.app.id).unwrap();
        if attempt == 0 {
            assert_eq!(value["success"], true);
            assert_eq!(rollouts.len(), 1);
            assert_eq!(rollouts[0].state, "running");
            first = Some(rollouts[0].id);
        } else if attempt == 1 {
            assert_eq!(value["success"], false);
            assert_eq!(value["errors"][0]["code"], 9_100_006);
            assert_eq!(
                value["errors"][0]["message"],
                "the request conflicts with current state"
            );
            assert_eq!(rollouts.len(), 1);
            assert_eq!(rollouts[0].id, first.unwrap());
            assert_eq!(rollouts[0].state, "running");
            assert_eq!(
                repo.rollout_progress(instance, authority.app.id, first.unwrap())
                    .unwrap(),
                (0, 0)
            );
        } else {
            assert_eq!(value["success"], true);
            assert_eq!(rollouts.len(), 2);
            assert_eq!(
                rollouts.iter().filter(|r| r.state == "completed").count(),
                1
            );
            assert_eq!(rollouts.iter().filter(|r| r.state == "running").count(), 1);
            assert_ne!(value["result"]["id"], first.unwrap().to_string());
        }
    }
}

async fn post_rollout(
    router: &Router,
    path: &str,
    authorization: &str,
    input: &Value,
) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header(header::AUTHORIZATION, authorization)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(input).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap();
    (status, value)
}

#[tokio::test]
async fn container_rollout_normalizes_official_step_percentage_and_fences_target_mismatch() {
    let fixture = fixture().await;
    let image = fixture.admit_image("rollout-step-percentage").await;
    let authority = authority(&fixture.service, &fixture._artifacts, image, true);
    let instance = fixture.service.instance_id();
    let repo = ContainerAuthority::new(fixture.service.storage.db());
    let configuration: Value = serde_json::from_slice(
        &repo
            .target_configuration(instance, authority.app.id, 1)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let initial_settings = repo
        .application_settings(instance, authority.app.id)
        .unwrap();
    let application = router(&fixture);
    let path = format!(
        "/accounts/{instance}/containers/applications/{}/rollouts",
        authority.app.id
    );
    // This is the exact original cf deploy immediate request shape, including
    // step_percentage rather than the selected management CLI's steps array.
    let input = serde_json::json!({"description":"Progressive update", "strategy":"rolling", "kind":"full_auto", "target_configuration":configuration, "step_percentage":100});
    let (status, _) = post_rollout(&application, &path, "Bearer read-token", &input).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    for selection in [
        serde_json::json!({}),
        serde_json::json!({"step_percentage":0}),
        serde_json::json!({"step_percentage":1}),
        serde_json::json!({"step_percentage":15}),
        serde_json::json!({"step_percentage":101}),
        serde_json::json!({"step_percentage":25.5}),
        serde_json::json!({"steps":[]}),
        serde_json::json!({"steps":[{"step_size":{"percentage":100}}],"step_percentage":100}),
    ] {
        let mut invalid = input.clone();
        invalid.as_object_mut().unwrap().remove("step_percentage");
        invalid
            .as_object_mut()
            .unwrap()
            .extend(selection.as_object().unwrap().clone());
        let (status, _) =
            post_rollout(&application, &path, "Bearer deployer-token", &invalid).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            repo.rollouts(instance, authority.app.id)
                .unwrap()
                .is_empty()
        );
    }
    let mut stale = input.clone();
    stale["target_configuration"]["image"] = serde_json::json!(format!(
        "registry.example.test/other@sha256:{}",
        "0".repeat(64)
    ));
    let (status, _) = post_rollout(&application, &path, "Bearer deployer-token", &stale).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        repo.rollouts(instance, authority.app.id)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        repo.application_settings(instance, authority.app.id)
            .unwrap(),
        initial_settings
    );
    for (index, percentage) in [5_u32, 10, 20, 25, 50, 100].into_iter().enumerate() {
        let mut request = input.clone();
        request["step_percentage"] = percentage.into();
        let (status, created) =
            post_rollout(&application, &path, "Bearer deployer-token", &request).await;
        assert_eq!(status, StatusCode::OK, "{created}");
        let id: ResourceId = created["result"]["id"].as_str().unwrap().parse().unwrap();
        let expected = (1..=100 / percentage)
            .map(|step| step * percentage)
            .collect::<Vec<_>>();
        assert_eq!(created["result"]["target_version"], 1);
        assert_eq!(created["result"]["target_configuration"], configuration);
        assert_eq!(created["result"]["progress"]["total_steps"], expected.len());
        let stored = repo
            .rollouts(instance, authority.app.id)
            .unwrap()
            .into_iter()
            .find(|rollout| rollout.id == id)
            .unwrap();
        assert_eq!(stored.steps, expected);
        assert_eq!(
            stored.details.description.as_deref(),
            Some("Progressive update")
        );
        assert!(stored.details.step_descriptions.iter().all(Option::is_none));
        for step in 0..stored.steps.len() {
            assert!(
                repo.rollout_step_leases(
                    instance,
                    authority.app.id,
                    id,
                    open_compute_core::wall_time_ms() + (index * 100 + step) as i64
                )
                .unwrap()
                .is_empty()
            );
        }
    }
    let mut explicit = input;
    explicit.as_object_mut().unwrap().remove("step_percentage");
    explicit["steps"] = serde_json::json!([{"description":"Half", "step_size":{"percentage":50}}, {"description":"All", "step_size":{"percentage":100}}]);
    let (status, created) =
        post_rollout(&application, &path, "Bearer deployer-token", &explicit).await;
    assert_eq!(status, StatusCode::OK);
    let last = repo
        .rollouts(instance, authority.app.id)
        .unwrap()
        .into_iter()
        .find(|rollout| rollout.id.to_string() == created["result"]["id"].as_str().unwrap())
        .unwrap();
    assert_eq!(last.steps, vec![50, 100]);
    assert_eq!(
        last.details.step_descriptions,
        vec![Some("Half".to_owned()), Some("All".to_owned())]
    );
}
