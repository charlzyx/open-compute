//! Cloudflare Containers applications and progressive rollout boundary.

use super::storage::{account, context, json, require_no_query, strict_query};
use super::{
    V4Error, V4Permission, V4ResourceKind, error_response, result_info_response, success_response,
};
use crate::containers::{
    ContainerService,
    wire::{ApplicationCreate, ApplicationPatch, Configuration, RolloutCreate},
};
use crate::http::HttpState;
use axum::Router;
use axum::extract::{Path, Request, State};
use axum::response::Response;
use axum::routing::get;
use open_compute_core::{ResourceId, ResourceState};
use open_compute_storage::{
    containers::{ContainerApplication, ContainerAuthority, ContainerRollout},
    durable_objects::DurableObjectRepository,
};
use std::sync::Arc;

pub(super) fn router() -> Router<HttpState> {
    Router::new().nest(
        "/accounts/{account_id}/containers",
        Router::new()
            .merge(super::container_instances::router())
            .route("/applications", get(list).post(create))
            .route("/me", get(account_limits))
            .route("/image-preparations", axum::routing::post(prepare_image))
            .route(
                "/registries/{domain}/credentials",
                axum::routing::post(registry_credentials),
            )
            .route(
                "/applications/{application_id}",
                get(application).patch(update).delete(delete_application),
            )
            .route(
                "/applications/{application_id}/rollouts",
                get(list_rollouts).post(create_rollout),
            )
            .route(
                "/applications/{application_id}/rollouts/{rollout_id}",
                get(rollout),
            ),
    )
}

pub(super) fn service(
    state: &HttpState,
    account_id: &str,
) -> Result<Arc<ContainerService>, V4Error> {
    let instance = account(state, account_id)?;
    state
        .container_service()
        .filter(|service| service.instance_id() == instance)
        .cloned()
        .ok_or(V4Error::Unavailable)
}

fn application_value(
    state: &HttpState,
    service: &ContainerService,
    app: &ContainerApplication,
) -> Result<serde_json::Value, V4Error> {
    let authority = state.v4_instance_context().ok_or(V4Error::Unavailable)?;
    let repo = ContainerAuthority::new(service.storage.db());
    if app.scheduling_policy
        == open_compute_storage::containers::ContainerSchedulingPolicy::DurableObject
    {
        let (configuration, logs, updated_at_ms) = repo
            .application_settings(service.instance_id(), app.id)
            .map_err(|error| V4Error::from(&error))?;
        if configuration.is_some() {
            return Err(V4Error::Unavailable);
        }
        let namespace = authority.public_resource_id(
            V4ResourceKind::DurableObjectNamespace,
            app.namespace_resource_id,
        );
        return Ok(
            serde_json::json!({"id":namespace,"account_id":service.instance_id(),"name":app.name,
            "scheduling_policy":"durable_object","durable_objects":{"namespace_id":namespace},
            "created_at":super::iso_timestamp(app.created_at_ms)?,"updated_at":super::iso_timestamp(updated_at_ms)?,
            "observability":{"logs":{"enabled":logs}}}),
        );
    }
    let (configuration, logs, updated_at_ms) = service
        .application_configuration(app)
        .map_err(|error| V4Error::from(&error))?;
    let target = repo
        .target(
            service.instance_id(),
            app.id,
            app.target_generation.ok_or(V4Error::Unavailable)?,
        )
        .map_err(|error| V4Error::from(&error))?;
    let public_configuration = serde_json::json!({"image":configuration.image,"vcpu":target.spec.resources.cpu_nanos as f64/1e9,"memory_mib":target.spec.resources.memory_bytes/(1<<20),"disk":{"size_mb":target.spec.resources.disk_bytes as f64/1e6},"observability":{"logs":{"enabled":logs}}});
    let leases = repo
        .application_leases(service.instance_id(), app.id)
        .map_err(|error| V4Error::from(&error))?;
    let active = leases
        .iter()
        .filter(|lease| lease.state == open_compute_core::containers::ContainerLeaseState::Running)
        .count();
    Ok(
        serde_json::json!({"id":app.id,"account_id":service.instance_id(),"created_at":super::iso_timestamp(app.created_at_ms)?,"updated_at":super::iso_timestamp(updated_at_ms)?,"version":target.generation,"name":app.name,"scheduling_policy":"default","configuration":public_configuration,"instances":active,"health":{"instances":{"active":active,"assigned":0},"errors":[]},"max_instances":app.max_instances,"durable_objects":{"namespace_id":authority.public_resource_id(V4ResourceKind::DurableObjectNamespace,app.namespace_resource_id)},"constraints":{"tiers":[1,2]},"rollout_active_grace_period":0}),
    )
}

async fn list(
    State(state): State<HttpState>,
    Path(account_id): Path<String>,
    request: Request,
) -> Response {
    let context = match context(&request, V4Permission::Read) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let result = (|| {
        let service = service(&state, &account_id)?;
        let query = strict_query(&request)?;
        if query
            .keys()
            .any(|name| !matches!(name.as_str(), "page_token" | "per_page" | "name" | "image"))
        {
            return Err(V4Error::InvalidRequest);
        }
        let mut applications = ContainerAuthority::new(service.storage.db())
            .applications(service.instance_id())
            .map_err(|error| V4Error::from(&error))?
            .iter()
            .map(|app| application_value(&state, &service, app))
            .collect::<Result<Vec<_>, _>>()?;
        applications.retain(|app| {
            query
                .get("name")
                .is_none_or(|name| app["name"].as_str() == Some(name.as_str()))
                && query.get("image").is_none_or(|image| {
                    app["configuration"]["image"].as_str() == Some(image.as_str())
                })
        });
        let (items, info) = page_items(&query, applications, None, 500)?;
        Ok((items, info))
    })();
    match result {
        Ok((items, info)) => result_info_response(context, items, info),
        Err(error) => error_response(error, context.request_id()),
    }
}

async fn application(
    State(state): State<HttpState>,
    Path((account_id, id)): Path<(String, String)>,
    request: Request,
) -> Response {
    let context = match context(&request, V4Permission::Read) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let result = (|| {
        require_no_query(&request)?;
        let service = service(&state, &account_id)?;
        let app = public_application(&state, &service, &id)?;
        application_value(&state, &service, &app)
    })();
    match result {
        Ok(value) => success_response(context, value),
        Err(error) => error_response(error, context.request_id()),
    }
}

fn live_application(
    service: &ContainerService,
    id: ResourceId,
) -> Result<ContainerApplication, V4Error> {
    ContainerAuthority::new(service.storage.db())
        .applications(service.instance_id())
        .map_err(|error| V4Error::from(&error))?
        .into_iter()
        .find(|app| app.id == id)
        .ok_or(V4Error::NotFound)
}

pub(super) fn public_application(
    state: &HttpState,
    service: &ContainerService,
    id: &str,
) -> Result<ContainerApplication, V4Error> {
    let authority = state.v4_instance_context().ok_or(V4Error::Unavailable)?;
    ContainerAuthority::new(service.storage.db())
        .applications(service.instance_id())
        .map_err(|error| V4Error::from(&error))?
        .into_iter()
        .find(|app| match app.scheduling_policy {
            open_compute_storage::containers::ContainerSchedulingPolicy::Default => {
                app.id.to_string() == id
            }
            open_compute_storage::containers::ContainerSchedulingPolicy::DurableObject => authority
                .matches_public_resource_id(
                    V4ResourceKind::DurableObjectNamespace,
                    app.namespace_resource_id,
                    id,
                ),
        })
        .ok_or(V4Error::NotFound)
}

async fn prepare_image(
    State(state): State<HttpState>,
    Path(account_id): Path<String>,
    request: Request,
) -> Response {
    let context = match context(&request, V4Permission::ProductWrite) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let result = async {
        require_no_query(&request)?;
        let service = service(&state, &account_id)?;
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Preparation {
            image: String,
        }
        let input: Preparation = json(request, context.request_id())
            .await
            .map_err(|_| V4Error::InvalidRequest)?;
        service
            .prepare_image(&input.image)
            .await
            .map_err(|error| V4Error::from(&error))?;
        Ok(serde_json::json!({"status":"ready"}))
    }
    .await;
    match result {
        Ok(value) => success_response(context, value),
        Err(error) => error_response(error, context.request_id()),
    }
}

async fn create(
    State(state): State<HttpState>,
    Path(account_id): Path<String>,
    request: Request,
) -> Response {
    let context = match context(&request, V4Permission::ProductWrite) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let result = async {
        require_no_query(&request)?;
        let service = service(&state, &account_id)?;
        let authority = state.v4_instance_context().ok_or(V4Error::Unavailable)?;
        let input: ApplicationCreate = json(request, context.request_id())
            .await
            .map_err(|_| V4Error::InvalidRequest)?;
        let namespaces = DurableObjectRepository::new(&service.storage)
            .list_namespaces(service.instance_id())
            .map_err(|error| V4Error::from(&error))?;
        let namespace = namespaces
            .into_iter()
            .find(|namespace| {
                namespace.resource.state == ResourceState::Ready
                    && authority.matches_public_resource_id(
                        V4ResourceKind::DurableObjectNamespace,
                        namespace.resource.id,
                        &input.durable_objects.namespace_id,
                    )
            })
            .ok_or(V4Error::NotFound)?;
        let app = service
            .create_application(&input, namespace.resource.id, now_ms())
            .await
            .map_err(|error| V4Error::from(&error))?;
        application_value(&state, &service, &app)
    }
    .await;
    match result {
        Ok(value) => success_response(context, value),
        Err(error) => error_response(error, context.request_id()),
    }
}

async fn update(
    State(state): State<HttpState>,
    Path((account_id, id)): Path<(String, String)>,
    request: Request,
) -> Response {
    let context = match context(&request, V4Permission::ProductWrite) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let result = async {
        require_no_query(&request)?;
        let service = service(&state, &account_id)?;
        let id = public_application(&state, &service, &id)?.id;
        let input: ApplicationPatch = json(request, context.request_id())
            .await
            .map_err(|_| V4Error::InvalidRequest)?;
        let app = service
            .update_application(id, &input, now_ms())
            .await
            .map_err(|error| V4Error::from(&error))?;
        application_value(&state, &service, &app)
    }
    .await;
    match result {
        Ok(value) => success_response(context, value),
        Err(error) => error_response(error, context.request_id()),
    }
}

fn rollout_value(
    service: &ContainerService,
    rollout: &ContainerRollout,
) -> Result<serde_json::Value, V4Error> {
    let repo = ContainerAuthority::new(service.storage.db());
    let parse = |generation| -> Result<Configuration, V4Error> {
        let bytes = repo
            .target_configuration(service.instance_id(), rollout.application_id, generation)
            .map_err(|e| V4Error::from(&e))?
            .ok_or(V4Error::Unavailable)?;
        serde_json::from_slice(&bytes).map_err(|_| V4Error::Unavailable)
    };
    let (total, updated) = repo
        .rollout_progress(service.instance_id(), rollout.application_id, rollout.id)
        .map_err(|e| V4Error::from(&e))?;
    let status = match rollout.state.as_str() {
        "completed" => "completed",
        "cancelled" => "replaced",
        _ => "progressing",
    };
    let steps=rollout.steps.iter().enumerate().map(|(i,pct)|serde_json::json!({"id":i+1,"description":rollout.details.step_descriptions[i].clone().unwrap_or_default(),"step_size":{"percentage":pct},"status":if i<rollout.current_step{"completed"}else if i==rollout.current_step{"progressing"}else{"pending"}})).collect::<Vec<_>>();
    let mut value = serde_json::json!({"id":rollout.id,"created_at":super::iso_timestamp(rollout.created_at_ms)?,"last_updated_at":super::iso_timestamp(rollout.updated_at_ms)?,"strategy":"rolling","kind":"full_auto","status":status,"current_configuration":parse(rollout.current_generation)?,"current_version":rollout.current_generation,"target_configuration":parse(rollout.target_generation)?,"target_version":rollout.target_generation,"steps":steps,"progress":{"current_step":rollout.current_step,"total_instances":total,"total_steps":rollout.steps.len(),"updated_instances":updated}});
    if let Some(description) = &rollout.details.description {
        value["description"] = description.clone().into();
    }
    if rollout.state == "failed" {
        value["health"] = serde_json::json!({"errors":[{"description":"Rollout requires operator reconciliation"}]});
    }
    Ok(value)
}

async fn list_rollouts(
    State(state): State<HttpState>,
    Path((account, id)): Path<(String, String)>,
    request: Request,
) -> Response {
    let ctx = match context(&request, V4Permission::Read) {
        Ok(ctx) => ctx,
        Err(e) => return e.into_response(),
    };
    let result = (|| {
        let service = service(&state, &account)?;
        let application: ResourceId = id.parse().map_err(|_| V4Error::NotFound)?;
        let query = strict_query(&request)?;
        if query
            .keys()
            .any(|k| !matches!(k.as_str(), "page_token" | "per_page"))
        {
            return Err(V4Error::InvalidRequest);
        }
        live_application(&service, application)?;
        let values = ContainerAuthority::new(service.storage.db())
            .rollouts(service.instance_id(), application)
            .map_err(|e| V4Error::from(&e))?
            .iter()
            .map(|r| rollout_value(&service, r))
            .collect::<Result<Vec<_>, _>>()?;
        page_items(&query, values, Some(100), 1000)
    })();
    match result {
        Ok((items, info)) => result_info_response(ctx, items, info),
        Err(e) => error_response(e, ctx.request_id()),
    }
}

async fn create_rollout(
    State(state): State<HttpState>,
    Path((account_id, id)): Path<(String, String)>,
    request: Request,
) -> Response {
    let context = match context(&request, V4Permission::ProductWrite) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let result = async {
        require_no_query(&request)?;
        let service = service(&state, &account_id)?;
        let application: ResourceId = id.parse().map_err(|_| V4Error::NotFound)?;
        let input: RolloutCreate = json(request, context.request_id())
            .await
            .map_err(|_| V4Error::InvalidRequest)?;
        let rollout_steps = input.validate().map_err(|error| V4Error::from(&error))?;
        let repo = ContainerAuthority::new(service.storage.db());
        let target_configuration =
            serde_json::to_vec(&input.target_configuration).map_err(|_| V4Error::InvalidRequest)?;
        let steps = rollout_steps
            .iter()
            .map(|step| step.step_size.percentage)
            .collect::<Vec<_>>();
        let rollout = repo
            .create_rollout(
                service.instance_id(),
                application,
                &open_compute_storage::containers::ContainerRolloutCreate {
                    id: ResourceId::generate(),
                    target_configuration: &target_configuration,
                    steps: &steps,
                    details: Some(&open_compute_storage::containers::ContainerRolloutDetails {
                        description: input.description.clone(),
                        step_descriptions: rollout_steps
                            .iter()
                            .map(|step| step.description.clone())
                            .collect(),
                    }),
                },
                now_ms(),
            )
            .map_err(|error| V4Error::from(&error))?;
        rollout_value(&service, &rollout)
    }
    .await;
    match result {
        Ok(value) => success_response(context, value),
        Err(error) => error_response(error, context.request_id()),
    }
}

async fn rollout(
    State(state): State<HttpState>,
    Path((account_id, application, id)): Path<(String, String, String)>,
    request: Request,
) -> Response {
    let context = match context(&request, V4Permission::Read) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let result = (|| {
        require_no_query(&request)?;
        let service = service(&state, &account_id)?;
        let application: ResourceId = application.parse().map_err(|_| V4Error::NotFound)?;
        let id: ResourceId = id.parse().map_err(|_| V4Error::NotFound)?;
        live_application(&service, application)?;
        ContainerAuthority::new(service.storage.db())
            .rollout(service.instance_id(), application, id)
            .map_err(|error| V4Error::from(&error))
            .and_then(|rollout| rollout_value(&service, &rollout))
    })();
    match result {
        Ok(value) => success_response(context, value),
        Err(error) => error_response(error, context.request_id()),
    }
}
fn now_ms() -> i64 {
    open_compute_core::wall_time_ms()
}

async fn delete_application(
    State(state): State<HttpState>,
    Path((account_id, id)): Path<(String, String)>,
    request: Request,
) -> Response {
    let context = match context(&request, V4Permission::ProductWrite) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let result = async {
        require_no_query(&request)?;
        drop(request);
        let service = service(&state, &account_id)?;
        let application = public_application(&state, &service, &id)?;
        service
            .delete_application(application.id)
            .await
            .map_err(|error| V4Error::from(&error))?;
        Ok(serde_json::json!({"id":id}))
    }
    .await;
    match result {
        Ok(value) => success_response(context, value),
        Err(error) => error_response(error, context.request_id()),
    }
}

pub(super) fn page_items(
    query: &std::collections::BTreeMap<String, String>,
    values: Vec<serde_json::Value>,
    default_size: Option<usize>,
    maximum: usize,
) -> Result<(Vec<serde_json::Value>, serde_json::Value), V4Error> {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    let token = query.get("page_token").cloned().unwrap_or_default();
    let offset = if token.is_empty() {
        0
    } else {
        let bytes = URL_SAFE_NO_PAD
            .decode(&token)
            .map_err(|_| V4Error::InvalidRequest)?;
        std::str::from_utf8(&bytes)
            .map_err(|_| V4Error::InvalidRequest)?
            .parse::<usize>()
            .map_err(|_| V4Error::InvalidRequest)?
    };
    if offset > values.len() {
        return Err(V4Error::InvalidRequest);
    }
    let per_page = match query.get("per_page") {
        Some(value) => {
            let size = value
                .parse::<usize>()
                .map_err(|_| V4Error::InvalidRequest)?;
            if !(1..=maximum).contains(&size) {
                return Err(V4Error::InvalidRequest);
            }
            size
        }
        None => {
            if let Some(size) = default_size {
                size
            } else if token.is_empty() {
                values.len().max(1)
            } else {
                100
            }
        }
    };
    let total = values.len();
    let items = values
        .into_iter()
        .skip(offset)
        .take(per_page)
        .collect::<Vec<_>>();
    let next = offset
        .checked_add(items.len())
        .ok_or(V4Error::InvalidRequest)?;
    Ok((
        items,
        serde_json::json!({"per_page":per_page,"page_token":token,"next_page_token":if next<total{URL_SAFE_NO_PAD.encode(next.to_string())}else{String::new()}}),
    ))
}

async fn account_limits(
    State(state): State<HttpState>,
    Path(account_id): Path<String>,
    request: Request,
) -> Response {
    let context = match context(&request, V4Permission::Read) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let result = (|| {
        require_no_query(&request)?;
        let service = service(&state, &account_id)?;
        let resources = service.provider.resources();
        Ok(
            serde_json::json!({"external_account_id":service.instance_id(),"limits":{"vcpu_per_deployment":resources.cpu_nanos as f64/1e9,"memory_mib_per_deployment":resources.memory_bytes/(1<<20),"disk_mb_per_deployment":resources.disk_bytes as f64/1e6,"max_instances":service.provider.max_instances()}}),
        )
    })();
    match result {
        Ok(value) => success_response(context, value),
        Err(error) => error_response(error, context.request_id()),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryCredentials {
    expiration_minutes: u32,
    permissions: Vec<String>,
}

async fn registry_credentials(
    State(state): State<HttpState>,
    Path((account_id, domain)): Path<(String, String)>,
    request: Request,
) -> Response {
    let context = match context(&request, V4Permission::ProductWrite) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let result=async {
        require_no_query(&request)?;
        let service=service(&state,&account_id)?;
        let host=request.headers().get(axum::http::header::HOST).and_then(|host|host.to_str().ok()).ok_or(V4Error::InvalidRequest)?;
        if host!=domain{return Err(V4Error::InvalidRequest);}
        let input:RegistryCredentials=json(request,context.request_id()).await.map_err(|_|V4Error::InvalidRequest)?;
        let push=input.permissions.iter().any(|p|p=="push");
        let pull=input.permissions.iter().any(|p|p=="pull");
        if !(1..=15).contains(&input.expiration_minutes)||input.permissions.is_empty()||input.permissions.len()>2||input.permissions.iter().any(|p|p!="push"&&p!="pull")||(input.permissions.len()==2&&input.permissions[0]==input.permissions[1]) {return Err(V4Error::InvalidRequest);}
        let password=format!("{}{}",ResourceId::generate(),ResourceId::generate()).replace('-',"");
        let expires=now_ms().checked_add(i64::from(input.expiration_minutes)*60_000).ok_or(V4Error::InvalidRequest)?;
        ContainerAuthority::new(service.storage.db()).issue_registry_token(service.instance_id(),&super::container_registry::password_hash(&password),push,pull,expires).map_err(|error|V4Error::from(&error))?;
        Ok(serde_json::json!({"account_id":service.instance_id(),"registry_host":domain,"username":service.instance_id(),"password":password}))
    }.await;
    match result {
        Ok(value) => success_response(context, value),
        Err(error) => error_response(error, context.request_id()),
    }
}

#[cfg(test)]
#[path = "container_rollouts_tests.rs"]
mod tests;
