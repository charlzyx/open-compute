//! Resolve exec capabilities from Engine metadata without retaining completed history.

use super::*;

pub(super) fn container_id(value: &Value, id: &str) -> Result<String, PlatformError> {
    if value.get("ID").and_then(Value::as_str) != Some(id) {
        return Err(invalid());
    }
    value
        .get("ContainerID")
        .and_then(Value::as_str)
        .filter(|id| opaque(id))
        .map(str::to_owned)
        .ok_or_else(invalid)
}

pub(super) async fn inspect(state: &BrokerState, id: &str) -> Result<String, PlatformError> {
    let (status, value) = state
        .engine
        .json(Method::GET, &format!("/exec/{id}/json"), Value::Null)
        .await?;
    if status != 200 {
        return Err(invalid());
    }
    container_id(&value, id)
}

pub(super) async fn resolve(
    state: &BrokerState,
    id: &str,
) -> Result<(String, String), PlatformError> {
    let container = inspect(state, id).await?;
    let (status, value) = state
        .engine
        .json(
            Method::GET,
            &format!("/containers/{container}/json"),
            Value::Null,
        )
        .await?;
    if status != 200
        || value.get("Id").and_then(Value::as_str) != Some(container.as_str())
        || value
            .pointer("/Config/Labels/dev.open-compute.container.role")
            .and_then(Value::as_str)
            != Some("application")
    {
        return Err(invalid());
    }
    let key = value
        .pointer("/Config/Labels/dev.open-compute.container.lease")
        .and_then(Value::as_str)
        .filter(|key| opaque(key))
        .ok_or_else(invalid)?;
    Ok((key.to_owned(), container))
}

pub(super) async fn verify(
    state: &BrokerState,
    entry: &LeaseEntry,
    container: &str,
) -> Result<(), PlatformError> {
    let key = &entry.lease.lease_key;
    let (status, value) = state
        .engine
        .json(Method::GET, &format!("/containers/{key}/json"), Value::Null)
        .await?;
    if status != 200 || value.get("Id").and_then(Value::as_str) != Some(container) {
        return Err(invalid());
    }
    verify_inspection(state, &value, key, &entry.lease, &entry.startup).await?;
    if entry.process_identity.as_ref() != Some(&process_identity(&value)?) {
        return Err(invalid());
    }
    Ok(())
}
