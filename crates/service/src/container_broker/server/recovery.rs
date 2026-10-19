//! Revoke orphaned private grants before a qualified Broker accepts new callers.

use super::*;

pub(super) async fn revoke(
    engine: &Engine,
    keys: &BTreeSet<String>,
    journal: &super::super::journal::Journal,
    sidecar_image: &str,
) -> Result<(), PlatformError> {
    for key in keys {
        if journal.contains(key)? {
            continue;
        }
        let name = format!("{key}-proxy");
        let (status, value) = engine
            .json(
                Method::GET,
                &format!("/containers/{name}/json"),
                Value::Null,
            )
            .await?;
        if status == 404 {
            continue;
        }
        if status != 200
            || value
                .get("Id")
                .and_then(Value::as_str)
                .is_none_or(|id| !opaque(id))
            || value.get("Image").and_then(Value::as_str) != Some(sidecar_image)
        {
            return Err(invalid());
        }
        let labels = value
            .pointer("/Config/Labels")
            .and_then(Value::as_object)
            .ok_or_else(invalid)?;
        if labels
            .get("dev.open-compute.container.lease")
            .and_then(Value::as_str)
            != Some(key)
            || labels
                .get("dev.open-compute.container.role")
                .and_then(Value::as_str)
                != Some("proxy")
            || ["owner", "start", "contract", "manifest"]
                .iter()
                .any(|suffix| {
                    labels
                        .get(&format!("dev.open-compute.container.{suffix}"))
                        .and_then(Value::as_str)
                        .is_none_or(|value| !opaque(value))
                })
        {
            return Err(invalid());
        }
        let host = value
            .get("HostConfig")
            .and_then(Value::as_object)
            .ok_or_else(invalid)?;
        if host.get("NetworkMode").and_then(Value::as_str) != Some("bridge")
            || host.get("CapAdd") != Some(&json!(["NET_ADMIN"]))
            || host
                .get("Privileged")
                .is_some_and(|value| value != &json!(false))
            || host.get("PidMode").is_some_and(|value| value != &json!(""))
            || [
                "Binds",
                "Mounts",
                "VolumesFrom",
                "Devices",
                "DeviceRequests",
            ]
            .iter()
            .any(|field| {
                host.get(*field)
                    .is_some_and(|value| !value.is_null() && value != &json!([]))
            })
            || value
                .get("Mounts")
                .is_some_and(|value| !value.is_null() && value != &json!([]))
        {
            return Err(invalid());
        }
        journal.begin(key)?;
        // These unpublished tokens revoke both previous control and callback grants.
        super::super::credentials::install(
            engine,
            &name,
            &super::super::BrokerControlCredentials::generate()?,
        )
        .await?;
        journal.clear(key)?;
    }
    Ok(())
}
