//! Private authority registration, process observations and cleanup.

use super::*;

pub(super) async fn dispatch(
    state: &BrokerState,
    request: Request,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<Response<Body>, PlatformError> {
    if request.uri().query().is_some() {
        return Err(invalid());
    }
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    if method == Method::POST && path.ends_with("/snapshot") {
        return snapshot::capture(state, request, &path, permit).await;
    }
    if method == Method::POST && path == "/open-compute/v1/images/check" {
        let bytes = read_body(request.into_body(), 16 << 10)
            .await
            .map_err(|_| invalid())?;
        let proof: super::super::contract::ImageProof =
            serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        proof.validate()?;
        let (status, image) = state
            .engine
            .json(
                Method::GET,
                &format!("/images/{}/json", proof.image),
                Value::Null,
            )
            .await?;
        if status == 404 {
            return response(404, Vec::new());
        }
        if status != 200
            || !policy::image_matches(
                &image,
                &proof.image,
                &proof.diff_ids,
                &state.contract.architecture,
            )
        {
            return Err(invalid());
        }
        let size = image
            .get("Size")
            .and_then(Value::as_u64)
            .ok_or_else(invalid)?;
        return response(
            200,
            serde_json::to_vec(&super::super::contract::ImageSize { size })
                .map_err(|_| invalid())?,
        );
    }
    if method == Method::POST
        && matches!(
            path.as_str(),
            "/open-compute/v1/leases" | "/open-compute/v1/leases/cleanup"
        )
    {
        let bytes = read_body(request.into_body(), 64 << 10)
            .await
            .map_err(|_| invalid())?;
        let lease: BrokerLease = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        return registration::register(state, lease, path.ends_with("/cleanup")).await;
    }
    if method == Method::GET && path == "/open-compute/v1/inventory" {
        return response(
            200,
            serde_json::to_vec(&*state.unknown.lock().await).map_err(|_| invalid())?,
        );
    }
    if method == Method::GET {
        let key = path
            .strip_prefix("/open-compute/v1/leases/")
            .filter(|key| opaque(key))
            .ok_or_else(invalid)?;
        let entry = state
            .leases
            .lock()
            .await
            .get(key)
            .cloned()
            .ok_or_else(invalid)?;
        let mut entry = entry.lock().await;
        let (status, value) = state
            .engine
            .json(Method::GET, &format!("/containers/{key}/json"), Value::Null)
            .await?;
        let running = if status == 200 {
            verify_inspection(state, &value, key, &entry.lease, &entry.startup).await?;
            let running = value
                .pointer("/State/Running")
                .and_then(Value::as_bool)
                .ok_or_else(invalid)?;
            if running {
                let identity = process_identity(&value)?;
                if entry
                    .process_identity
                    .as_ref()
                    .is_some_and(|previous| previous != &identity)
                {
                    return Err(invalid());
                }
                entry.process_identity = Some(identity);
            }
            running
        } else if status == 404 {
            false
        } else {
            return Err(invalid());
        };
        let start_identity = entry
            .process_identity
            .as_ref()
            .map(|identity| process_start_identity(key, identity))
            .unwrap_or_default();
        let observation = ProviderLeaseObservation {
            start_identity,
            image: entry.lease.image.clone(),
            manifest_sha256: entry.lease.manifest_sha256.clone(),
            provider_contract_sha256: entry.lease.provider_contract_sha256.clone(),
            running,
            terminated: entry.cleaned,
            cleanup_complete: entry.cleaned,
            late_operations_fenced: entry.fenced,
        };
        return response(
            200,
            serde_json::to_vec(&observation).map_err(|_| invalid())?,
        );
    }
    if method == Method::POST && path.ends_with("/credentials") {
        return credentials(state, request, &path).await;
    }
    let graceful = method == Method::POST && path.ends_with("/stop");
    if method == Method::DELETE || graceful {
        return terminate(state, request, &path, graceful).await;
    }
    if method == Method::POST && path == "/open-compute/v1/images/load" {
        let (file, size) = super::super::spool::stage(
            request.into_body(),
            &state.config.spool_dir,
            state.config.max_image_bytes,
        )
        .await?;
        let imported = state
            .engine
            .send_archive("/images/load?quiet=true", file, size)
            .await?;
        if imported.status().as_u16() != 200 {
            return Err(invalid());
        }
        let bytes = bounded_response(imported, 1 << 20).await?;
        super::super::engine::verify_load_response(&bytes)?;
        return response(200, Vec::new());
    }
    Err(invalid())
}

async fn terminate(
    state: &BrokerState,
    request: Request,
    path: &str,
    graceful: bool,
) -> Result<Response<Body>, PlatformError> {
    let graceful_ms = if graceful {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Stop {
            grace_millis: u64,
        }
        let bytes = read_body(request.into_body(), 1_024)
            .await
            .map_err(|_| invalid())?;
        let stop: Stop = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        if stop.grace_millis > 900_000 {
            return Err(invalid());
        }
        Some(stop.grace_millis)
    } else {
        None
    };
    let key = (if graceful {
        path.strip_suffix("/stop").ok_or_else(invalid)?
    } else {
        path
    })
    .strip_prefix("/open-compute/v1/leases/")
    .filter(|key| opaque(key))
    .ok_or_else(invalid)?;
    let entry = state
        .leases
        .lock()
        .await
        .get(key)
        .cloned()
        .ok_or_else(invalid)?;
    let mut entry = entry.lock().await;
    entry.fenced = true;
    entry.generation_cancel.cancel();
    entry.relay = None;
    if let Some(grace) = graceful_ms {
        verify_existing(state, key, &entry.lease, &entry.startup).await?;
        let stop_path = format!(
            "/containers/{key}/stop?t={}&signal=SIGTERM",
            grace.div_ceil(1_000)
        );
        let operation = state.engine.json(Method::POST, &stop_path, Value::Null);
        let (status, _) =
            tokio::time::timeout(std::time::Duration::from_millis(grace + 5_000), operation)
                .await
                .map_err(|_| invalid())??;
        if !matches!(status, 204 | 304 | 404) {
            return Err(invalid());
        }
    }
    for name in [key.to_owned(), format!("{key}-proxy")] {
        verify_existing(state, &name, &entry.lease, &entry.startup).await?;
        let (status, _) = state
            .engine
            .json(
                Method::DELETE,
                &format!("/containers/{name}?force=true"),
                Value::Null,
            )
            .await?;
        if !matches!(status, 204 | 404) {
            return Err(invalid());
        }
        let (status, _) = state
            .engine
            .json(
                Method::GET,
                &format!("/containers/{name}/json"),
                Value::Null,
            )
            .await?;
        if status != 404 {
            return Err(invalid());
        }
    }
    state.journal.clear(key)?;
    state.unknown.lock().await.remove(key);
    entry.cleaned = true;
    entry.relay = None;
    response(204, Vec::new())
}

async fn credentials(
    state: &BrokerState,
    request: Request,
    path: &str,
) -> Result<Response<Body>, PlatformError> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Generation {
        generation_sha256: String,
    }
    let key = path
        .strip_prefix("/open-compute/v1/leases/")
        .and_then(|path| path.strip_suffix("/credentials"))
        .filter(|key| opaque(key))
        .ok_or_else(invalid)?;
    let bytes = read_body(request.into_body(), 1_024).await?;
    let generation: Generation = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if !opaque(&generation.generation_sha256) {
        return Err(invalid());
    }
    let entry = state
        .leases
        .lock()
        .await
        .get(key)
        .cloned()
        .ok_or_else(invalid)?;
    let mut entry = entry.lock().await;
    if entry.fenced {
        return Err(invalid());
    }
    if entry.generation_sha256.as_ref() != Some(&generation.generation_sha256) {
        // Revoke the former grant before I/O; a failed overwrite must not return it.
        entry.generation_cancel.cancel();
        entry.generation_cancel = state.shutdown.child_token();
        entry.relay = None;
        entry.credentials = None;
        entry.generation_sha256 = None;
        let credentials = super::super::BrokerControlCredentials::generate()?;
        let name = format!("{key}-proxy");
        verify_existing(state, &name, &entry.lease, &entry.startup).await?;
        let (status, _) = state
            .engine
            .json(
                Method::GET,
                &format!("/containers/{name}/json"),
                Value::Null,
            )
            .await?;
        if status == 200 {
            state.journal.begin(key)?;
            entry.fenced = true;
            super::super::credentials::install(&state.engine, &name, &credentials).await?;
            state.journal.clear(key)?;
            entry.fenced = false;
        } else if status != 404 {
            return Err(invalid());
        }
        entry.credentials = Some(credentials);
        entry.generation_sha256 = Some(generation.generation_sha256);
    }
    response(
        200,
        serde_json::to_vec(entry.credentials.as_ref().ok_or_else(invalid)?)
            .map_err(|_| invalid())?,
    )
}
