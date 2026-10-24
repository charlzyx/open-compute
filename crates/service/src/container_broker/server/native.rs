//! Closed native engine dispatch and streaming protocol checks.

use super::*;

pub(super) async fn dispatch(
    state: Arc<BrokerState>,
    mut request: Request,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<Response<Body>, PlatformError> {
    let operation = policy::operation(request.method(), request.uri())?;
    let is_exec = matches!(
        operation,
        Operation::ExecStart(_) | Operation::ExecInspect(_) | Operation::ExecResize(_)
    );
    let execution = if is_exec {
        Some(exec::resolve(&state, operation.identity().ok_or_else(invalid)?).await?)
    } else {
        None
    };
    let entry = if let Some(key) = operation.identity() {
        let key = if let Some((key, _)) = &execution {
            key.clone()
        } else {
            key.strip_suffix("-proxy").unwrap_or(key).to_owned()
        };
        Some(
            state
                .leases
                .lock()
                .await
                .get(&key)
                .cloned()
                .ok_or_else(invalid)?,
        )
    } else {
        None
    };
    let (lease, startup) = if let Some(entry) = &entry {
        let entry = entry.lock().await;
        if entry.fenced && operation.starts_process() {
            return Err(invalid());
        }
        (Some(entry.lease.clone()), Some(entry.startup.clone()))
    } else {
        (None, None)
    };
    if operation.starts_process() && !state.unknown.lock().await.is_empty() {
        return Err(invalid());
    }
    let exec_start = matches!(operation, Operation::ExecStart(_));
    let upgrade = request.headers().contains_key("Upgrade");
    if (upgrade && !exec_start)
        || (upgrade
            && request
                .headers()
                .get("Upgrade")
                .is_none_or(|value| !value.as_bytes().eq_ignore_ascii_case(b"tcp")))
    {
        return Err(invalid());
    }
    let client_upgrade = if upgrade {
        Some(hyper::upgrade::on(&mut request))
    } else {
        None
    };
    let method = request.method().clone();
    let mut uri = request.uri().to_string();
    let bytes = read_body(request.into_body(), 16 << 20)
        .await
        .map_err(|_| invalid())?;
    let body = match &operation {
        Operation::Create(name) => {
            let startup = startup.as_ref().ok_or_else(invalid)?;
            let body = policy::prepare_create(
                &bytes,
                name,
                lease.as_ref().ok_or_else(invalid)?,
                state.config.sidecar_image.as_deref().ok_or_else(invalid)?,
                startup.environment(),
                &state.config.exec_helper_volume,
            )?;
            startup.prepare_create(&body, name)?
        }
        Operation::ExecCreate(_) => startup.as_ref().ok_or_else(invalid)?.prepare_exec(&bytes)?,
        Operation::ExecStart(_) => {
            let value = policy::validate_exec(&bytes, true)?;
            if value
                .get("Detach")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                == upgrade
            {
                return Err(invalid());
            }
            bytes.to_vec()
        }
        Operation::Archive(_) => {
            super::super::credentials::validate_ca_archive(&bytes)?;
            bytes.to_vec()
        }
        _ if bytes.is_empty() => Vec::new(),
        _ => return Err(invalid()),
    };
    let mut guard = if let Some(entry) = &entry {
        Some(entry.lock().await)
    } else {
        None
    };
    if guard
        .as_ref()
        .is_some_and(|entry| entry.fenced && operation.starts_process())
    {
        return Err(invalid());
    }
    if operation.starts_process()
        && guard
            .as_ref()
            .is_none_or(|entry| entry.credentials.is_none())
    {
        return Err(invalid());
    }
    if let Operation::Delete(name) = &operation
        && let Some(entry) = &mut guard
    {
        if name.ends_with("-proxy") {
            return delete_proxy(&state, entry, name).await;
        }
        directory_start::fence_main_removal(&state, entry).await?;
    }

    let existing = if let (Some(lease), Some(name)) = (&lease, operation.identity())
        && !is_exec
        && !matches!(operation, Operation::Create(_))
    {
        verify_existing(&state, name, lease, startup.as_ref().ok_or_else(invalid)?).await?
    } else {
        None
    };
    if let Some((_, container)) = &execution {
        let id = operation.identity().ok_or_else(invalid)?;
        if exec::inspect(&state, id).await? != *container {
            return Err(invalid());
        }
        exec::verify(&state, guard.as_ref().ok_or_else(invalid)?, container).await?;
    } else if matches!(operation, Operation::ExecCreate(_)) {
        let (container, _) = existing.as_ref().ok_or_else(invalid)?;
        exec::verify(&state, guard.as_ref().ok_or_else(invalid)?, container).await?;
        uri = format!("/containers/{container}/exec");
    }
    helper::before(&state, &operation, existing.as_ref()).await?;
    if matches!(operation, Operation::Wait(_)) {
        let Some((id, _)) = existing else {
            return response(
                404,
                b"{\"message\":\"Container provider operation rejected\"}".to_vec(),
            );
        };
        let query = uri.find('?').map_or("", |offset| &uri[offset..]);
        uri = format!("/containers/{id}/wait{query}");
        guard = None;
    }

    let tty = if upgrade {
        serde_json::from_slice::<Value>(&body)
            .map_err(|_| invalid())?
            .get("Tty")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    } else {
        false
    };
    if let Some(entry) = guard.as_deref_mut() {
        directory_start::before(&state, entry, &operation).await?;
    }
    let engine_response = state.engine.send(method, &uri, body, upgrade).await?;
    let status = engine_response.status().as_u16();
    if status == 101 {
        if !upgrade {
            return Err(invalid());
        }
        let upstream = engine_response.upgrade().await.map_err(|_| invalid())?;
        let client_upgrade = client_upgrade.ok_or_else(invalid)?;
        let maximum = state.config.max_stream_bytes;

        let cancellation = guard
            .as_ref()
            .ok_or_else(invalid)?
            .generation_cancel
            .clone();
        tokio::spawn(async move {
            let _permit = permit;
            let client = tokio::select! {
                result = client_upgrade => result,
                () = cancellation.cancelled() => return,
            };
            if let Ok(client) = client {
                let client = hyper_util::rt::TokioIo::new(client);
                let (mut client_read, mut client_write) = tokio::io::split(client);
                let (mut upstream_read, mut upstream_write) = tokio::io::split(upstream);
                tokio::select! {
                    _ = async {
                        let _ = tokio::try_join!(
                            super::super::relay::bounded_copy(&mut client_read, &mut upstream_write, maximum),
                            super::super::relay::framed_copy(&mut upstream_read, &mut client_write, maximum, tty));
                    } => {},
                    () = cancellation.cancelled() => {},
                }
            }
        });
        return Response::builder()
            .status(101)
            .header("Connection", "Upgrade")
            .header("Upgrade", "tcp")
            .body(Body::empty())
            .map_err(|_| invalid());
    }
    let mut bytes = if matches!(operation, Operation::Wait(_)) {
        super::super::engine::wait_response(engine_response, 16 << 20).await?
    } else {
        bounded_response(engine_response, 16 << 20).await?
    };
    if !accepted_status(&operation, status) {
        return Err(invalid());
    }
    helper::created(&state, &operation, status, guard.as_deref_mut()).await?;
    if let Some(entry) = guard.as_deref_mut() {
        directory_start::after(&state, entry, &operation, status).await?;
    }

    if let Operation::ExecCreate(_) = operation
        && status == 201
    {
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        value
            .get("Id")
            .and_then(Value::as_str)
            .filter(|id| opaque(id))
            .ok_or_else(invalid)?;
    }
    if let Operation::ExecInspect(id) = &operation
        && status == 200
        && exec::container_id(&serde_json::from_slice(&bytes).map_err(|_| invalid())?, id)?
            != execution.as_ref().ok_or_else(invalid)?.1
    {
        return Err(invalid());
    }

    if let Operation::Inspect(name) = &operation
        && status == 200
    {
        let mut value: Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        verify_inspection(
            &state,
            &value,
            name,
            lease.as_ref().ok_or_else(invalid)?,
            startup.as_ref().ok_or_else(invalid)?,
        )
        .await?;
        if name.ends_with("-proxy")
            && value.pointer("/State/Running").and_then(Value::as_bool) == Some(true)
        {
            let port = super::super::relay::published_port(&value)?;
            let entry = guard.as_mut().ok_or_else(invalid)?;
            if entry.fenced || entry.credentials.is_none() {
                return Err(invalid());
            }
            if entry
                .relay
                .as_ref()
                .is_none_or(|relay| relay.target_port() != port)
            {
                entry.relay = Some(
                    super::super::relay::Relay::start(
                        state.config.relay_host,
                        port,
                        state.relay_requests.clone(),
                        entry.generation_cancel.clone(),
                    )
                    .await?,
                );
            }
            entry
                .relay
                .as_ref()
                .ok_or_else(invalid)?
                .rewrite(&mut value)?;
            bytes = serde_json::to_vec(&value).map_err(|_| invalid())?;
        }
    }

    response(
        status,
        if status >= 400 {
            b"{\"message\":\"Container provider operation rejected\"}".to_vec()
        } else {
            bytes
        },
    )
}

async fn delete_proxy(
    state: &BrokerState,
    entry: &mut LeaseEntry,
    name: &str,
) -> Result<Response<Body>, PlatformError> {
    if verify_existing(state, &entry.lease.lease_key, &entry.lease, &entry.startup)
        .await?
        .is_some_and(|(_, running)| running)
    {
        return Err(invalid());
    }
    let existing = verify_existing(state, name, &entry.lease, &entry.startup).await?;
    let was_fenced = entry.fenced;
    entry.generation_cancel.cancel();
    entry.relay = None;
    let Some((id, _)) = existing else {
        if !was_fenced {
            entry.generation_cancel = state.shutdown.child_token();
        }
        return response(
            404,
            b"{\"message\":\"Container provider operation rejected\"}".to_vec(),
        );
    };
    // Unknown deletion cannot authorize a replacement after this request or Broker restarts.
    entry.fenced = true;
    if !state.journal.contains(&entry.lease.lease_key)? {
        state.journal.begin(&entry.lease.lease_key)?;
    }
    let (status, _) = state
        .engine
        .json(
            Method::DELETE,
            &format!("/containers/{id}?force=true"),
            Value::Null,
        )
        .await?;
    if !matches!(status, 204 | 404) {
        return Err(invalid());
    }
    for identity in [id.as_str(), name] {
        let (status, _) = state
            .engine
            .json(
                Method::GET,
                &format!("/containers/{identity}/json"),
                Value::Null,
            )
            .await?;
        if status != 404 {
            return Err(invalid());
        }
    }
    if !was_fenced {
        state.journal.clear(&entry.lease.lease_key)?;
        entry.fenced = false;
        entry.generation_cancel = state.shutdown.child_token();
    }
    response(status, Vec::new())
}
