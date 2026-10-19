//! Exec environments derive from the same complete image/startup authority as create.

use super::*;

#[test]
fn startup_and_exec_share_cloudflare_environment_names_and_bounded_values() {
    let variables = vec![
        "A-B=hyphen".into(),
        "A.B=dot".into(),
        "9NUM=digits-first".into(),
        "_NAME=value=with=equals".into(),
        format!("{}=long-name", "N".repeat(257)),
    ];
    let mut lease = lease();
    lease.startup.environment_variables = variables.clone();
    lease.startup.validate().unwrap();
    let effective = EffectiveStartup::from_image(&json!({"Config":{}}), &lease).unwrap();
    let mut request = create(false);
    request["Env"] = json!(variables);
    let created: Value = serde_json::from_slice(
        &prepare_create(
            &serde_json::to_vec(&request).unwrap(),
            &lease.lease_key,
            &lease,
            contract().sidecar_image.as_deref().unwrap(),
            effective.environment(),
            &config().exec_helper_volume,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(created["Env"], json!(variables));
    let exec: Value = serde_json::from_slice(
        &effective
            .prepare_exec(&serde_json::to_vec(&json!({"Cmd":["program"],"Env":variables})).unwrap())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        &exec["Env"].as_array().unwrap()[variables.len() + 2..],
        json!(variables).as_array().unwrap()
    );
    for variables in [
        vec!["A-B=first".into(), "A-B=second".into()],
        vec!["变量=unicode".into()],
        vec!["with space=value".into()],
        vec!["=".into()],
        vec!["A\0B=value".into()],
        vec!["A=value\0".into()],
        vec!["missing-equals".into()],
        vec!["=empty-name".into()],
        vec![format!("A={}", "x".repeat(32_767))],
        (0..513).map(|index| format!("A{index}=value")).collect(),
    ] {
        lease.startup.environment_variables = variables.clone();
        assert!(lease.startup.validate().is_err());
        assert!(
            effective
                .prepare_exec(
                    &serde_json::to_vec(&json!({"Cmd":["program"],"Env":variables})).unwrap()
                )
                .is_err()
        );
    }
}

#[test]
fn native_signal_routes_preserve_all_canonical_linux_signal_numbers() {
    let key = lease().lease_key;
    for signal in 1..=64 {
        assert_eq!(
            operation(
                &Method::POST,
                &format!("/containers/{key}/kill?signal={signal}")
                    .parse()
                    .unwrap()
            )
            .unwrap(),
            Operation::Kill(key.clone())
        );
    }
    for signal in [
        "0", "65", "255", "256", "-1", "+15", "015", "SIGTERM", "SIGKILL", "", "1.0",
    ] {
        assert!(
            operation(
                &Method::POST,
                &format!("/containers/{key}/kill?signal={signal}")
                    .parse()
                    .unwrap()
            )
            .is_err()
        );
    }
}
use crate::container_broker::startup::EffectiveStartup;

#[test]
fn exec_removes_inherited_environment_and_preserves_explicit_overrides() {
    let lease = lease();
    let effective = EffectiveStartup::from_image(
        &json!({"Config":{"Env":["IMAGE=default", "PATH=/image/bin", "HOME=/image-home", "HOSTNAME=image-host", "TERM=image-term", "case=lower"]}}),
        &lease,
    )
    .unwrap();
    for environment in [None, Some(json!([]))] {
        let mut request = json!({"Cmd":["env"],"Tty":true});
        if let Some(environment) = environment {
            request["Env"] = environment;
        }
        let prepared: Value = serde_json::from_slice(
            &effective
                .prepare_exec(&serde_json::to_vec(&request).unwrap())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(prepared["Cmd"], request["Cmd"]);
        assert_eq!(prepared["Tty"], true);
        assert_eq!(
            prepared["Env"],
            json!(["HOME", "HOSTNAME", "IMAGE", "KEY", "TERM", "case"])
        );
    }
    let overrides = json!([
        "KEY=exec",
        "IMAGE=",
        "PATH=/exec/bin",
        "HOME=/exec-home",
        "HOSTNAME=exec-host",
        "TERM=exec-term",
        "case=exec",
        "NEW=value"
    ]);
    let request = json!({"Cmd":["env"],"Env":overrides,"User":"1000","WorkingDir":"/exec"});
    let prepared: Value = serde_json::from_slice(
        &effective
            .prepare_exec(&serde_json::to_vec(&request).unwrap())
            .unwrap(),
    )
    .unwrap();
    let environment = prepared["Env"].as_array().unwrap();
    assert_eq!(&environment[6..], overrides.as_array().unwrap());
    assert_eq!(prepared["User"], "1000");
    assert_eq!(prepared["WorkingDir"], "/exec");
    assert!(
        !environment
            .iter()
            .any(|value| value.as_str().unwrap().contains("image-home"))
    );
    for invalid in [
        json!({"Cmd":["env"],"Env":vec!["A=value";513]}),
        json!({"Cmd":["env"],"Env":["A=\u{0000}"]}),
        json!({"Cmd":["env"],"Env":true}),
        json!({"Cmd":["env"],"Privileged":true}),
        json!({"Cmd":[]}),
    ] {
        assert!(
            effective
                .prepare_exec(&serde_json::to_vec(&invalid).unwrap())
                .is_err()
        );
    }
    let maximum = json!({"Cmd":["env"],"Env":(0..512).map(|index| format!("A{index}=value")).collect::<Vec<_>>()});
    assert!(
        effective
            .prepare_exec(&serde_json::to_vec(&maximum).unwrap())
            .is_ok()
    );
}

#[test]
fn duplicate_image_environment_is_normalized_before_create_and_exec() {
    for startup in [Vec::new(), vec!["KEY=startup".into()]] {
        let mut lease = lease();
        lease.startup.environment_variables = startup;
        let image = json!({"Config":{"Env":["KEY=image-first", "IMAGE=first", "KEY=image-last", "IMAGE=last", "PATH=/first", "PATH=/last"]}});
        let effective = EffectiveStartup::from_image(&image, &lease).unwrap();
        let expected = if lease.startup.environment_variables.is_empty() {
            json!(["KEY=image-last", "IMAGE=last", "PATH=/last"])
        } else {
            // Moby first appends each absent image key; runc then keeps the last occurrence.
            json!(["KEY=startup", "IMAGE=first", "PATH=/first"])
        };
        assert_eq!(json!(effective.environment()), expected);
        let mut request = create(false);
        request["Env"] = json!(lease.startup.environment_variables);
        let created: Value = serde_json::from_slice(
            &prepare_create(
                &serde_json::to_vec(&request).unwrap(),
                &lease.lease_key,
                &lease,
                contract().sidecar_image.as_deref().unwrap(),
                effective.environment(),
                &config().exec_helper_volume,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(created["Env"], expected);
        let exec: Value = serde_json::from_slice(
            &effective
                .prepare_exec(&serde_json::to_vec(&json!({"Cmd":["env"]})).unwrap())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(exec["Env"], json!(["HOSTNAME", "IMAGE", "KEY", "TERM"]));
    }
}

#[test]
fn inherited_image_environment_cannot_expand_past_the_engine_request_budget() {
    let mut lease = lease();
    lease.startup.environment_variables.clear();
    let environment: Vec<_> = (0..1_024)
        .map(|index| format!("KEY_{index}_{}=value", "a".repeat(16_400)))
        .collect();
    let effective =
        EffectiveStartup::from_image(&json!({"Config":{"Env":environment}}), &lease).unwrap();
    assert!(effective.prepare_exec(br#"{"Cmd":["env"]}"#).is_err());
    let mut request = create(false);
    request["Env"] = json!([]);
    assert!(
        prepare_create(
            &serde_json::to_vec(&request).unwrap(),
            &lease.lease_key,
            &lease,
            contract().sidecar_image.as_deref().unwrap(),
            effective.environment(),
            &config().exec_helper_volume,
        )
        .is_err()
    );
}
