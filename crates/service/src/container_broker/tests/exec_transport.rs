//! Real Unix transport uses current Engine exec ownership rather than lifetime history.

use super::*;

pub(super) async fn running() -> (RestartBroker, QualifiedContainerProvider, BrokerLease) {
    let mut broker = RestartBroker::new().await;
    // One registered lease made the former lifetime history limit128.
    broker.config.max_leases = 1;
    let provider = broker.start().await;
    let mut lease = lease();
    lease.provider_contract_sha256 = provider.contract_sha256().to_owned();
    lease.max_instances = 1;
    provider.register(&lease).await.unwrap();
    provider
        .lease_credentials(&lease.lease_key, &"1".repeat(64))
        .await
        .unwrap();
    let client = broker.client();
    for proxy in [true, false] {
        let name = format!("{}{}", lease.lease_key, if proxy { "-proxy" } else { "" });
        assert_eq!(
            client
                .post(format!("http://localhost/containers/create?name={name}"))
                .body(serde_json::to_vec(&create(proxy)).unwrap())
                .send()
                .await
                .unwrap()
                .status(),
            201
        );
        assert_eq!(
            client
                .post(format!("http://localhost/containers/{name}/start"))
                .send()
                .await
                .unwrap()
                .status(),
            204
        );
    }
    assert!(provider.observe(&lease.lease_key).await.unwrap().running);
    (broker, provider, lease)
}

async fn create_exec(client: &reqwest::Client, lease: &BrokerLease) -> String {
    let response = client
        .post(format!(
            "http://localhost/containers/{}/exec",
            lease.lease_key
        ))
        .body(serde_json::to_vec(&json!({"Cmd":["program"]})).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    serde_json::from_slice::<Value>(&response.bytes().await.unwrap()).unwrap()["Id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn inspect_exec(client: &reqwest::Client, id: &str) -> reqwest::Response {
    client
        .get(format!("http://localhost/exec/{id}/json"))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn completed_exec_history_does_not_exhaust_capacity_or_invalidate_retained_handles() {
    let (mut broker, provider, lease) = running().await;
    let client = broker.client();
    let mut first = None;
    for _ in 0..130 {
        let id = create_exec(&client, &lease).await;
        first.get_or_insert(id.clone());
        assert_eq!(
            client
                .post(format!("http://localhost/exec/{id}/start"))
                .body(serde_json::to_vec(&json!({"Detach":true,"Tty":false})).unwrap())
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        let response = inspect_exec(&client, &id).await;
        assert_eq!(response.status(), 200);
        let value: Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(value["ExitCode"], 0);
        assert_eq!(value["Running"], false);
    }
    for _ in 0..2 {
        assert_eq!(
            inspect_exec(&client, first.as_ref().unwrap())
                .await
                .status(),
            200
        );
    }
    provider.terminate(&lease.lease_key).await.unwrap();
    assert_eq!(
        inspect_exec(&client, first.as_ref().unwrap())
            .await
            .status(),
        503
    );
    assert!(broker.state.containers.lock().await.is_empty());
    broker.retire().await;
}

#[tokio::test]
async fn exec_identity_rejects_foreign_orphan_and_changed_main_before_mutation() {
    let (mut broker, provider, lease) = running().await;
    let client = broker.client();
    let id = create_exec(&client, &lease).await;
    let original = broker.state.containers.lock().await[&lease.lease_key].clone();
    let original_exec = broker.state.executions.lock().await[&id].clone();
    for forged in [
        json!({"ID":id,"ContainerID":"7".repeat(64),"Running":false,"ExitCode":0}),
        json!({"ID":"6".repeat(64),"ContainerID":original["Id"],"Running":false,"ExitCode":0}),
        json!({"ID":id,"ContainerID":"bad","Running":false,"ExitCode":0}),
    ] {
        broker
            .state
            .executions
            .lock()
            .await
            .insert(id.clone(), forged);
        assert_rejected_without_exec_mutation(&broker, &client, &id).await;
    }
    broker
        .state
        .executions
        .lock()
        .await
        .insert(id.clone(), original_exec);
    let foreign_id = "7".repeat(64);
    let mut foreign = original.clone();
    foreign["Id"] = json!(foreign_id);
    foreign["Config"]["Labels"]["dev.open-compute.container.lease"] = json!("6".repeat(64));
    broker
        .state
        .containers
        .lock()
        .await
        .insert("foreign".into(), foreign);
    broker.state.executions.lock().await.get_mut(&id).unwrap()["ContainerID"] = json!(foreign_id);
    assert_rejected_without_exec_mutation(&broker, &client, &id).await;
    broker.state.containers.lock().await.remove("foreign");
    broker.state.executions.lock().await.get_mut(&id).unwrap()["ContainerID"] =
        original["Id"].clone();
    for (pointer, value) in [
        ("/Id", json!("5".repeat(64))),
        ("/State/StartedAt", json!("new-process")),
        ("/Image", json!("sha256:".to_owned() + &"4".repeat(64))),
        (
            "/Config/Labels/dev.open-compute.container.start",
            json!("3".repeat(64)),
        ),
        ("/Config/Env", json!(["UNADMITTED=value"])),
    ] {
        let mut changed = original.clone();
        *changed.pointer_mut(pointer).unwrap() = value;
        broker
            .state
            .containers
            .lock()
            .await
            .insert(lease.lease_key.clone(), changed);
        assert_rejected_without_exec_mutation(&broker, &client, &id).await;
    }
    broker
        .state
        .containers
        .lock()
        .await
        .insert(lease.lease_key.clone(), original);
    assert_eq!(inspect_exec(&client, &id).await.status(), 200);
    provider.terminate(&lease.lease_key).await.unwrap();
    broker.retire().await;
}

async fn assert_rejected_without_exec_mutation(
    broker: &RestartBroker,
    client: &reqwest::Client,
    id: &str,
) {
    let before = broker.state.requests.lock().await.len();
    assert_eq!(inspect_exec(client, id).await.status(), 503);
    assert_eq!(
        client
            .post(format!("http://localhost/exec/{id}/start"))
            .body(serde_json::to_vec(&json!({"Detach":true,"Tty":false})).unwrap())
            .send()
            .await
            .unwrap()
            .status(),
        503
    );
    assert_eq!(
        client
            .post(format!("http://localhost/exec/{id}/resize?h=24&w=80"))
            .send()
            .await
            .unwrap()
            .status(),
        503
    );
    assert!(
        !broker.state.requests.lock().await[before..]
            .iter()
            .any(|request| request.starts_with("POST /exec/"))
    );
}

#[tokio::test]
async fn exec_identity_rechecks_under_lease_guard_after_delayed_engine_inspection() {
    // The full main/proxy proof and downstream exec operation share termination's lease lock.
    {
        let (mut broker, provider, lease) = running().await;
        let client = broker.client();
        let id = create_exec(&client, &lease).await;
        let gate = Arc::new(tokio::sync::Notify::new());
        *broker.state.inspect_gate.lock().await = Some(gate.clone());
        let inspecting = tokio::spawn(async move { inspect_exec(&client, &id).await });
        broker.state.inspect_started.notified().await;
        let key = lease.lease_key.clone();
        let terminating = tokio::spawn(async move { provider.terminate(&key).await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            !terminating.is_finished(),
            "termination overtook exec's ownership proof"
        );
        gate.notify_one();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), inspecting)
                .await
                .unwrap()
                .unwrap()
                .status(),
            200
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), terminating)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(broker.state.containers.lock().await.is_empty());
        broker.retire().await;
    }
    let (mut broker, provider, lease) = running().await;
    let client = broker.client();
    let id = create_exec(&client, &lease).await;
    let gate = Arc::new(tokio::sync::Notify::new());
    *broker.state.exec_inspect_gate.lock().await = Some(gate.clone());
    let pending_client = client.clone();
    let pending_id = id.clone();
    let starting = tokio::spawn(async move {
        pending_client
            .post(format!("http://localhost/exec/{pending_id}/start"))
            .body(serde_json::to_vec(&json!({"Detach":true,"Tty":false})).unwrap())
            .send()
            .await
            .unwrap()
    });
    broker.state.exec_inspect_started.notified().await;
    provider.terminate(&lease.lease_key).await.unwrap();
    gate.notify_one();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(5), starting)
            .await
            .unwrap()
            .unwrap()
            .status(),
        503
    );
    assert!(
        !broker
            .state
            .requests
            .lock()
            .await
            .iter()
            .any(|request| request == &format!("POST /exec/{id}/start"))
    );
    assert!(broker.state.containers.lock().await.is_empty());
    broker.retire().await;
}

#[tokio::test]
async fn environment_names_reject_invalid_exec_before_engine_mutation() {
    let (mut broker, provider, lease) = running().await;
    let client = broker.client();
    let url = format!("http://localhost/containers/{}/exec", lease.lease_key);
    let before = broker.state.exec_requests.lock().await.len();
    for variable in [
        "变量=value",
        "with space=value",
        "=empty-name",
        "=",
        "missing-equals",
        "A\0B=value",
        "A=value\0",
    ] {
        let response = client
            .post(&url)
            .body(serde_json::to_vec(&json!({"Cmd":["program"],"Env":[variable]})).unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 503);
        assert_eq!(broker.state.exec_requests.lock().await.len(), before);
    }
    let environment = vec![
        "A-B=hyphen".to_owned(),
        "A.B=dot".to_owned(),
        "9NUM=digits-first".to_owned(),
        "_NAME=value=with=equals".to_owned(),
        format!("{}=long-name", "N".repeat(257)),
    ];
    assert_eq!(
        client
            .post(&url)
            .body(serde_json::to_vec(&json!({"Cmd":["program"],"Env":environment})).unwrap())
            .send()
            .await
            .unwrap()
            .status(),
        201
    );
    let expected = json!(environment);
    {
        let requests = broker.state.exec_requests.lock().await;
        assert_eq!(requests.len(), before + 1);
        assert!(
            requests.last().unwrap()["Env"]
                .as_array()
                .unwrap()
                .ends_with(expected.as_array().unwrap())
        );
    }
    provider.terminate(&lease.lease_key).await.unwrap();
    assert!(broker.state.containers.lock().await.is_empty());
    broker.retire().await;
}
