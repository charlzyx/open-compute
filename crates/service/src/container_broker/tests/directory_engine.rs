//! Owned directory Engine objects and deterministic one-shot helper outcomes.

use super::*;

pub(super) async fn handle(
    state: &MockEngine,
    method: &Method,
    path: &str,
    bytes: &[u8],
) -> Option<Response<Body>> {
    let (status, value) = if path == "/volumes" && method == Method::GET {
        (
            200,
            json!({"Volumes":state.directory_volumes.lock().await.values().cloned().collect::<Vec<_>>(),"Warnings":[]}),
        )
    } else if path == "/volumes/create" && method == Method::POST {
        let mut volume: Value = serde_json::from_slice(bytes).unwrap();
        let name = volume["Name"].as_str().unwrap().to_owned();
        assert!(name.starts_with("oc-directory-"));
        volume["Scope"] = json!("local");
        volume["Options"] = Value::Null;
        let mut volumes = state.directory_volumes.lock().await;
        let volume = volumes.entry(name).or_insert(volume).clone();
        (201, volume)
    } else {
        let name = path.strip_prefix("/volumes/oc-directory-")?;
        let name = format!("oc-directory-{name}");
        if method == Method::GET {
            state
                .directory_volumes
                .lock()
                .await
                .get(&name)
                .cloned()
                .map_or((404, Value::Null), |value| (200, value))
        } else if method == Method::DELETE {
            let containers = state.containers.lock().await;
            let mounted = containers.values().any(|container| {
                container["Mounts"]
                    .as_array()
                    .is_some_and(|mounts| mounts.iter().any(|mount| mount["Name"] == name))
            });
            if mounted {
                (409, Value::Null)
            } else {
                (
                    if state.directory_volumes.lock().await.remove(&name).is_some() {
                        204
                    } else {
                        404
                    },
                    Value::Null,
                )
            }
        } else {
            (400, Value::Null)
        }
    };
    Some(
        Response::builder()
            .status(status)
            .body(Body::from(if value.is_null() {
                Vec::new()
            } else {
                serde_json::to_vec(&value).unwrap()
            }))
            .unwrap(),
    )
}

pub(super) fn start(value: &mut Value, exit: u64) {
    let quota = value
        .pointer("/Config/Labels/dev.open-compute.container.role")
        .and_then(Value::as_str)
        == Some("directory-helper");
    value["State"]["Running"] = (!quota).into();
    if !quota {
        value["State"]["Status"] = json!("running");
        if value.pointer("/State/StartedAt").and_then(Value::as_str) == Some("0001-01-01T00:00:00Z")
        {
            value["State"]["StartedAt"] = json!("2026-10-10T00:00:00Z");
        }
    }
    if quota {
        value["State"]["Status"] = json!("exited");
        value["State"]["ExitCode"] = json!(exit);
    }
}

// Docker 29.4 rejects relative symlink targets outside its archive extraction root.
pub(super) fn parent_link_escapes(bytes: &[u8]) -> bool {
    use std::path::Component;
    let mut archive = tar::Archive::new(bytes);
    for entry in archive.entries().unwrap() {
        let entry = entry.unwrap();
        if !entry.header().entry_type().is_symlink() {
            continue;
        }
        let target = entry.link_name().unwrap().unwrap();
        if target.is_absolute() {
            continue;
        }
        let path = entry.path().unwrap();
        let mut depth = path
            .parent()
            .unwrap()
            .components()
            .filter(|c| matches!(c, Component::Normal(_)))
            .count();
        for part in target.components() {
            match part {
                Component::ParentDir if depth == 0 => return true,
                Component::ParentDir => depth -= 1,
                Component::Normal(_) => depth += 1,
                _ => {}
            }
        }
    }
    false
}
