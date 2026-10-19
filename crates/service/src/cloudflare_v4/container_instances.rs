//! Public Container instance observations derived only from durable execution authority.

use super::containers::{page_items, public_application, service};
use super::storage::{context, require_no_query, strict_query};
use super::{V4Error, V4Permission, error_response, result_info_response, success_response};
use crate::containers::ContainerService;
use crate::http::HttpState;
use axum::{
    Router,
    extract::{Path, Request, State},
    response::Response,
    routing::get,
};
use open_compute_core::containers::ContainerLeaseState;
use open_compute_storage::containers::{ContainerAuthority, ContainerLease};
use std::collections::BTreeMap;

pub(super) fn router() -> Router<HttpState> {
    Router::new()
        .route("/applications/{application_id}/instances-v2", get(list))
        .route(
            "/applications/{application_id}/instances/{instance_id}",
            get(instance),
        )
}

fn instances(
    state: &HttpState,
    service: &ContainerService,
    application: &str,
) -> Result<Vec<serde_json::Value>, V4Error> {
    let id = public_application(state, service, application)?.id;
    let repo = ContainerAuthority::new(service.storage.db());
    let mut latest: BTreeMap<String, ContainerLease> = BTreeMap::new();
    for lease in repo
        .application_leases(service.instance_id(), id)
        .map_err(|e| V4Error::from(&e))?
    {
        let key = lease.identity.object_id.to_string();
        if latest.get(&key).is_none_or(|old| {
            (
                old.identity.object_generation,
                old.identity.lease_generation,
            ) < (
                lease.identity.object_generation,
                lease.identity.lease_generation,
            )
        }) {
            latest.insert(key, lease);
        }
    }
    latest
        .into_values()
        .map(|lease| value(state, service, &lease))
        .collect()
}

fn value(
    state: &HttpState,
    service: &ContainerService,
    lease: &ContainerLease,
) -> Result<serde_json::Value, V4Error> {
    let app = ContainerAuthority::new(service.storage.db())
        .applications(service.instance_id())
        .map_err(|error| V4Error::from(&error))?
        .into_iter()
        .find(|app| app.id == lease.application_id)
        .ok_or(V4Error::NotFound)?;
    let image = if lease.startup.image_reference.is_empty() {
        format!("sha256:{}", hex::encode(lease.effective_image_sha256))
    } else {
        lease.startup.image_reference.clone()
    };
    let lease_state = match lease.state {
        ContainerLeaseState::Admitting | ContainerLeaseState::Starting => "provisioning",
        ContainerLeaseState::Running => "running",
        ContainerLeaseState::Stopping => "stopping",
        ContainerLeaseState::Stopped => "stopped",
        ContainerLeaseState::Lost => "unknown",
    };
    let result = serde_json::json!({"id":lease.identity.object_id.to_string(),"application_id":if app.scheduling_policy == open_compute_storage::containers::ContainerSchedulingPolicy::DurableObject { state.v4_instance_context().ok_or(V4Error::Unavailable)?.public_resource_id(super::V4ResourceKind::DurableObjectNamespace, app.namespace_resource_id) } else { app.id.to_string() },
        "image":image,"status":{"state":lease_state,"updated_at":super::iso_timestamp(lease.updated_at_ms)?},
        "configuration":{"vcpu":lease.effective_resources.cpu_nanos as f64/1e9,"memory":lease.effective_resources.memory_bytes/(1<<20),"disk":lease.effective_resources.disk_bytes as f64/1e6}});

    Ok(result)
}

async fn list(
    State(state): State<HttpState>,
    Path((account, application)): Path<(String, String)>,
    request: Request,
) -> Response {
    let ctx = match context(&request, V4Permission::Read) {
        Ok(ctx) => ctx,
        Err(e) => return e.into_response(),
    };
    let result = (|| {
        let service = service(&state, &account)?;
        let query = strict_query(&request)?;
        if query.keys().any(|k| {
            !matches!(
                k.as_str(),
                "page_token" | "per_page" | "state" | "name_prefix"
            )
        }) {
            return Err(V4Error::InvalidRequest);
        }
        if query
            .get("state")
            .is_some_and(|s| !matches!(s.as_str(), "active" | "not-active"))
        {
            return Err(V4Error::InvalidRequest);
        }
        let mut values = instances(&state, &service, &application)?;
        values.retain(|v| {
            query.get("name_prefix").is_none_or(|prefix| {
                v["name"]
                    .as_str()
                    .or_else(|| v["id"].as_str())
                    .is_some_and(|name| name.starts_with(prefix))
            }) && query.get("state").is_none_or(|s| {
                let actual = v["status"]["state"].as_str().unwrap_or("");
                if s == "active" {
                    matches!(actual, "provisioning" | "running" | "stopping")
                } else {
                    matches!(actual, "stopped" | "failed")
                }
            })
        });
        page_items(&query, values, Some(100), 1000)
    })();
    match result {
        Ok((values, info)) => result_info_response(ctx, values, info),
        Err(e) => error_response(e, ctx.request_id()),
    }
}

async fn instance(
    State(state): State<HttpState>,
    Path((account, application, id)): Path<(String, String, String)>,
    request: Request,
) -> Response {
    let ctx = match context(&request, V4Permission::Read) {
        Ok(ctx) => ctx,
        Err(e) => return e.into_response(),
    };
    let result = (|| {
        require_no_query(&request)?;
        let service = service(&state, &account)?;
        instances(&state, &service, &application)?
            .into_iter()
            .find(|v| v["id"].as_str() == Some(&id))
            .ok_or(V4Error::NotFound)
    })();
    match result {
        Ok(value) => success_response(ctx, value),
        Err(e) => error_response(e, ctx.request_id()),
    }
}

#[cfg(test)]
#[path = "container_instances_tests.rs"]
mod tests;
