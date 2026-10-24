//! Real Unix Engine fixture for qualification and restricted capture transitions.

use super::*;

fn qualification_archive(container: &Value, failure: Option<&str>) -> Vec<u8> {
    let nonce = container["Config"]["Env"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .find_map(|value| value.strip_prefix("OPEN_COMPUTE_QUALIFICATION_NONCE="))
        .unwrap();
    let mut builder = tar::Builder::new(Vec::new());
    let root = if failure == Some("content") {
        "wrong"
    } else {
        nonce
    };
    let entries = if failure == Some("volatile") {
        vec![
            ("open-compute-snapshot-probe", root),
            ("run/open-compute-snapshot-probe", nonce),
        ]
    } else if failure == Some("helper") {
        vec![
            ("open-compute-snapshot-probe", root),
            (".open-compute-exec/helper", "trusted helper"),
        ]
    } else {
        vec![("open-compute-snapshot-probe", root)]
    };
    for (name, content) in entries {
        let mut header = tar::Header::new_ustar();
        header.set_mode(if failure == Some("mode") {
            0o644
        } else {
            0o640
        });
        header.set_size(content.len() as u64);
        header.set_cksum();
        builder
            .append_data(&mut header, name, content.as_bytes())
            .unwrap();
    }
    builder.into_inner().unwrap()
}

pub(super) async fn handle(
    state: &MockEngine,
    method: &Method,
    path: &str,
    query: &BTreeMap<String, String>,
) -> Option<Response<Body>> {
    if method == Method::GET
        && path.ends_with("/archive")
        && query.get("path").map(String::as_str) == Some("/open-compute-snapshot-probe")
    {
        let container = path
            .strip_prefix("/containers/")
            .unwrap()
            .strip_suffix("/archive")
            .unwrap();
        state.marker_archive_calls.fetch_add(1, Ordering::SeqCst);
        let error = {
            let mut error = state.marker_archive_error.lock().await;
            if state
                .marker_archive_error_once
                .swap(false, Ordering::SeqCst)
            {
                error.take()
            } else {
                error.clone()
            }
        };
        if let Some((status, body)) = error {
            if state.marker_archive_exited.swap(false, Ordering::SeqCst) {
                let mut containers = state.containers.lock().await;
                let value = containers
                    .values_mut()
                    .find(|value| value["Id"] == container)
                    .unwrap();
                value["State"]["Running"] = json!(false);
            }
            return Some(
                Response::builder()
                    .status(status)
                    .body(Body::from(body))
                    .unwrap(),
            );
        }
        let containers = state.containers.lock().await;
        let value = containers
            .values()
            .find(|value| value["Id"] == container)
            .unwrap();
        return Some(
            Response::builder()
                .status(200)
                .body(Body::from(qualification_archive(value, None)))
                .unwrap(),
        );
    }

    let (container, operation) = path.strip_prefix("/containers/")?.split_once('/')?;
    if !(method == Method::POST && matches!(operation, "pause" | "unpause")
        || method == Method::GET && operation == "export"
        || method == Method::GET
            && operation == "archive"
            && query.get("path").map(String::as_str) == Some("/app/data/."))
    {
        return None;
    }
    let gate = {
        let mut gate = state.snapshot_gate.lock().await;
        if gate.as_ref().is_some_and(|(stage, _)| *stage == operation) {
            gate.take()
        } else {
            None
        }
    };
    if let Some((_, gate)) = gate {
        state.snapshot_started.notify_one();
        gate.notified().await;
    }
    if *state.snapshot_failure.lock().await == Some(operation) {
        return Some(Response::builder().status(500).body(Body::empty()).unwrap());
    }
    let mut containers = state.containers.lock().await;
    let (name, value) = containers
        .iter_mut()
        .find(|(_, value)| value["Id"] == container)?;
    let qualification = name.starts_with("oc-qualification-");
    if operation == "export" || operation == "archive" {
        assert_eq!(value["State"]["Paused"], operation == "export");
        let archive = if qualification {
            qualification_archive(value, *state.snapshot_qualification_failure.lock().await)
        } else {
            state.snapshot_archive.lock().await.clone().unwrap()
        };
        return Some(
            Response::builder()
                .status(200)
                .body(Body::from(archive))
                .unwrap(),
        );
    }
    value["State"]["Paused"] = json!(operation == "pause");
    if operation == "unpause"
        && (state.snapshot_changed_start.swap(false, Ordering::SeqCst)
            || qualification
                && *state.snapshot_qualification_failure.lock().await == Some("identity"))
    {
        value["State"]["StartedAt"] = json!("2026-10-11T00:00:00Z");
    }
    Some(Response::builder().status(204).body(Body::empty()).unwrap())
}
