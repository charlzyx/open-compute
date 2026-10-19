//! Frozen entrypoint overrides use Docker's entrypoint and discard the image command.

use super::*;

#[tokio::test]
async fn frozen_entrypoint_replaces_image_defaults_and_rejects_foreign_create_or_inspect() {
    for override_value in [
        None,
        Some(Vec::new()),
        Some(vec!["/replacement".into(), "argument".into()]),
    ] {
        let mut broker = RestartBroker::new().await;
        let provider = broker.start().await;
        let mut lease = lease();
        lease.startup.entrypoint = override_value.clone();
        lease.max_instances = broker.config.max_instances;
        lease.provider_contract_sha256 = provider.contract_sha256().into();
        provider.register(&lease).await.unwrap();
        provider
            .lease_credentials(&lease.lease_key, &"1".repeat(64))
            .await
            .unwrap();
        let client = broker.client();
        let proxy = format!("{}-proxy", lease.lease_key);
        assert_eq!(
            client
                .post(format!("http://localhost/containers/create?name={proxy}"))
                .body(serde_json::to_vec(&create(true)).unwrap())
                .send()
                .await
                .unwrap()
                .status(),
            201
        );
        assert_eq!(
            client
                .post(format!("http://localhost/containers/{proxy}/start"))
                .send()
                .await
                .unwrap()
                .status(),
            204
        );
        let mut body = create(false);
        let explicit = override_value
            .as_ref()
            .is_some_and(|value| !value.is_empty());
        if explicit {
            body["Entrypoint"] = json!(override_value);
        } else {
            body.as_object_mut().unwrap().remove("Entrypoint");
            body.as_object_mut().unwrap().remove("Cmd");
        }
        let before = broker.state.containers.lock().await.len();
        for field in ["Cmd", "Entrypoint"] {
            let mut foreign = body.clone();
            foreign[field] = json!(["foreign"]);
            assert_eq!(
                client
                    .post(format!(
                        "http://localhost/containers/create?name={}",
                        lease.lease_key
                    ))
                    .body(serde_json::to_vec(&foreign).unwrap())
                    .send()
                    .await
                    .unwrap()
                    .status(),
                503
            );
            assert_eq!(broker.state.containers.lock().await.len(), before);
        }
        if explicit {
            for field in ["Cmd", "Entrypoint"] {
                let mut missing = body.clone();
                missing.as_object_mut().unwrap().remove(field);
                assert_eq!(
                    client
                        .post(format!(
                            "http://localhost/containers/create?name={}",
                            lease.lease_key
                        ))
                        .body(serde_json::to_vec(&missing).unwrap())
                        .send()
                        .await
                        .unwrap()
                        .status(),
                    503
                );
            }
        }
        assert_eq!(
            client
                .post(format!(
                    "http://localhost/containers/create?name={}",
                    lease.lease_key
                ))
                .body(serde_json::to_vec(&body).unwrap())
                .send()
                .await
                .unwrap()
                .status(),
            201
        );
        let (expected_entrypoint, expected_command) = if explicit {
            (json!(["/replacement", "argument"]), json!([]))
        } else {
            (json!(["image-entrypoint"]), json!(["image-command"]))
        };
        {
            let containers = broker.state.containers.lock().await;
            let actual = &containers[&lease.lease_key]["Config"];
            assert_eq!(actual["Entrypoint"], expected_entrypoint);
            assert_eq!(actual["Cmd"], expected_command);
        }
        let start = format!("http://localhost/containers/{}/start", lease.lease_key);
        for (field, expected) in [
            ("Cmd", &expected_command),
            ("Entrypoint", &expected_entrypoint),
        ] {
            broker
                .state
                .containers
                .lock()
                .await
                .get_mut(&lease.lease_key)
                .unwrap()["Config"][field] = json!(["foreign"]);
            assert_eq!(client.post(&start).send().await.unwrap().status(), 503);
            assert!(
                !broker.state.containers.lock().await[&lease.lease_key]["State"]["Running"]
                    .as_bool()
                    .unwrap()
            );
            broker
                .state
                .containers
                .lock()
                .await
                .get_mut(&lease.lease_key)
                .unwrap()["Config"][field] = expected.clone();
        }
        assert_eq!(client.post(&start).send().await.unwrap().status(), 204);
        assert!(provider.observe(&lease.lease_key).await.unwrap().running);
        provider.terminate(&lease.lease_key).await.unwrap();
        assert!(broker.state.containers.lock().await.is_empty());
        broker.retire().await;
    }
}
