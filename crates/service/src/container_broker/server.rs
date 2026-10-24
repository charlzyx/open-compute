//! Private HTTP serving, identity fencing and bounded Docker dispatch.

use super::{
    contract::{
        BrokerConfig, BrokerLease, ProviderContract, ProviderLeaseObservation, invalid, opaque,
        process_identity, process_start_identity,
    },
    engine::{Engine, bounded_response},
    policy::{self, Operation},
    socket::BrokerSocket,
};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{Method, Response, StatusCode},
    routing::any,
};
use open_compute_core::PlatformError;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::IntoFuture,
    sync::Arc,
};
use tokio::sync::{Mutex, Semaphore};

mod control;
mod directory;
mod directory_archive;
mod directory_start;
mod exec;
mod helper;
mod native;
mod recovery;
mod registration;
mod snapshot;

struct LeaseEntry {
    directories_ready: bool,
    directory_archives: directory::Archives,
    lease: BrokerLease,
    startup: super::startup::EffectiveStartup,
    fenced: bool,
    cleaned: bool,
    process_identity: Option<(String, String)>,
    relay: Option<super::relay::Relay>,
    credentials: Option<super::BrokerControlCredentials>,
    generation_sha256: Option<String>,
    generation_cancel: tokio_util::sync::CancellationToken,
}

struct BrokerState {
    config: BrokerConfig,
    contract: ProviderContract,
    engine: Engine,
    journal: super::journal::Journal,
    leases: Mutex<BTreeMap<String, Arc<Mutex<LeaseEntry>>>>,
    unknown: Mutex<BTreeSet<String>>,
    requests: Arc<Semaphore>,
    control_requests: Arc<Semaphore>,
    relay_requests: Arc<Semaphore>,
    shutdown: tokio_util::sync::CancellationToken,
}

pub(super) async fn run(
    config: BrokerConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), PlatformError> {
    config.validate()?;
    if config.peer_uid != rustix::process::getuid().as_raw() {
        return Err(invalid());
    }
    let journal = super::journal::Journal::open(&config.state_dir, config.max_leases)?;
    let mut socket = BrokerSocket::bind(config.socket.clone(), config.peer_uid)?;
    let engine = Engine::new(&config.engine_socket, "/_ping").await?;
    let contract = engine.qualify(&config).await?;
    let mut unknown = inventory(&engine, config.max_leases).await?;
    unknown.extend(journal.keys()?);
    if unknown.len() > config.max_leases as usize {
        return Err(invalid());
    }
    if contract.qualified {
        recovery::revoke(
            &engine,
            &unknown,
            &journal,
            config.sidecar_image.as_deref().ok_or_else(invalid)?,
        )
        .await?;
    }
    let cancellation = tokio_util::sync::CancellationToken::new();
    let state = Arc::new(BrokerState {
        relay_requests: Arc::new(Semaphore::new(config.relay_capacity()?)),
        requests: Arc::new(Semaphore::new(config.max_requests as usize)),
        control_requests: Arc::new(Semaphore::new(config.max_requests as usize)),
        shutdown: cancellation.clone(),
        config,
        contract,
        engine,
        journal,
        leases: Mutex::new(BTreeMap::new()),
        unknown: Mutex::new(unknown),
    });
    let app = Router::new()
        .fallback(any(dispatch))
        .with_state(state.clone());
    let listener = socket.listener.take().ok_or_else(invalid)?;
    let cancel = cancellation.clone();
    let shutdown_task = tokio::spawn(async move {
        shutdown.await;
        cancel.cancel();
    });
    let serving = axum::serve(
        TrustedListener {
            listener,
            uid: state.config.peer_uid,
        },
        app,
    )
    .with_graceful_shutdown(cancellation.clone().cancelled_owned())
    .into_future();
    tokio::pin!(serving);
    let result = tokio::select! {
        result = &mut serving => result.map_err(|_| invalid()),
        () = cancellation.cancelled() => {
            match tokio::time::timeout(std::time::Duration::from_secs(5), &mut serving).await {
                Ok(result) => result.map_err(|_| invalid()),
                Err(_) => Ok(()),
            }
        }
    };
    shutdown_task.abort();
    result
}

struct TrustedListener {
    listener: tokio::net::UnixListener,
    uid: u32,
}

impl axum::serve::Listener for TrustedListener {
    type Io = tokio::net::UnixStream;
    type Addr = tokio::net::unix::SocketAddr;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.listener.accept().await {
                Ok((stream, address))
                    if stream.peer_cred().is_ok_and(|peer| peer.uid() == self.uid) =>
                {
                    return (stream, address);
                }
                Ok(_) => {}
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
            }
        }
    }
    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

async fn dispatch(State(state): State<Arc<BrokerState>>, request: Request) -> Response<Body> {
    let pool = if request.uri().path().starts_with("/open-compute/") {
        &state.control_requests
    } else {
        &state.requests
    };
    let Ok(permit) = pool.clone().try_acquire_owned() else {
        return failure(StatusCode::TOO_MANY_REQUESTS);
    };
    let cancellation = state.shutdown.clone();
    let result = tokio::select! {
        result = dispatch_inner(state, request, permit) => result,
        () = cancellation.cancelled() => Err(invalid()),
    };
    match result {
        Ok(response) => response,
        Err(_) => failure(StatusCode::SERVICE_UNAVAILABLE),
    }
}

async fn dispatch_inner(
    state: Arc<BrokerState>,
    request: Request,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<Response<Body>, PlatformError> {
    let path = request.uri().path();
    if path == "/open-compute/v1/contract"
        && request.method() == Method::GET
        && request.uri().query().is_none()
    {
        state.engine.ensure_active().await?;
        return response(
            200,
            serde_json::to_vec(&state.contract).map_err(|_| invalid())?,
        );
    }
    if !state.contract.qualified {
        return Err(invalid());
    }
    if path.starts_with("/open-compute/") {
        return control::dispatch(&state, request, permit).await;
    }
    native::dispatch(state, request, permit).await
}

async fn verify_existing(
    state: &BrokerState,
    name: &str,
    lease: &BrokerLease,
    startup: &super::startup::EffectiveStartup,
) -> Result<Option<(String, bool)>, PlatformError> {
    let (status, value) = state
        .engine
        .json(
            Method::GET,
            &format!("/containers/{name}/json"),
            Value::Null,
        )
        .await?;
    if status == 404 {
        return Ok(None);
    }
    if status != 200 {
        return Err(invalid());
    }
    verify_inspection(state, &value, name, lease, startup).await?;
    let id = value
        .get("Id")
        .and_then(Value::as_str)
        .filter(|id| opaque(id))
        .ok_or_else(invalid)?;
    let running = value
        .pointer("/State/Running")
        .and_then(Value::as_bool)
        .ok_or_else(invalid)?;
    Ok(Some((id.to_owned(), running)))
}

async fn verify_inspection(
    state: &BrokerState,
    value: &Value,
    name: &str,
    lease: &BrokerLease,
    startup: &super::startup::EffectiveStartup,
) -> Result<(), PlatformError> {
    let image = state.config.sidecar_image.as_deref().ok_or_else(invalid)?;
    let sidecar_id = if name.ends_with("-proxy") {
        None
    } else {
        let sidecar_name = format!("{}-proxy", lease.lease_key);
        let (status, sidecar) = state
            .engine
            .json(
                Method::GET,
                &format!("/containers/{sidecar_name}/json"),
                Value::Null,
            )
            .await?;
        if status != 200 {
            return Err(invalid());
        }
        policy::verify_inspect(
            &sidecar,
            &sidecar_name,
            lease,
            image,
            None,
            &state.config.exec_helper_volume,
        )?;
        Some(
            sidecar
                .get("Id")
                .and_then(Value::as_str)
                .filter(|id| opaque(id))
                .ok_or_else(invalid)?
                .to_owned(),
        )
    };
    policy::verify_inspect(
        value,
        name,
        lease,
        image,
        sidecar_id.as_deref(),
        &state.config.exec_helper_volume,
    )?;
    if !name.ends_with("-proxy") {
        directory::verify_volumes(state, lease).await?;
    }
    startup.verify(value, name)
}

async fn inventory(engine: &Engine, maximum: u32) -> Result<BTreeSet<String>, PlatformError> {
    let filters = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("all", "true")
        .append_pair(
            "filters",
            "{\"label\":[\"dev.open-compute.container.lease\"]}",
        )
        .finish();
    let (status, value) = engine
        .json(
            Method::GET,
            &format!("/containers/json?{filters}"),
            Value::Null,
        )
        .await?;
    if status != 200 {
        return Err(invalid());
    }
    let list = value
        .as_array()
        .filter(|list| list.len() <= maximum as usize * 3)
        .ok_or_else(invalid)?;
    let mut result = BTreeSet::new();
    for entry in list {
        let key = entry
            .pointer("/Labels/dev.open-compute.container.lease")
            .and_then(Value::as_str)
            .filter(|key| opaque(key))
            .ok_or_else(invalid)?;
        result.insert(key.to_owned());
    }
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair(
            "filters",
            "{\"label\":[\"dev.open-compute.container.lease\"]}",
        )
        .finish();
    let (status, volumes) = engine
        .json(Method::GET, &format!("/volumes?{query}"), Value::Null)
        .await?;
    if status != 200 {
        return Err(invalid());
    }
    let volumes = volumes.get("Volumes").ok_or_else(invalid)?;
    if !volumes.is_null() {
        for volume in volumes.as_array().ok_or_else(invalid)? {
            let key = volume
                .pointer("/Labels/dev.open-compute.container.lease")
                .and_then(Value::as_str)
                .filter(|key| opaque(key))
                .ok_or_else(invalid)?;
            let index = volume
                .pointer("/Labels/dev.open-compute.container.directory.index")
                .and_then(Value::as_str)
                .ok_or_else(invalid)?;
            let position = index.parse::<usize>().map_err(|_| invalid())?;
            if index != position.to_string()
                || volume.get("Name").and_then(Value::as_str)
                    != Some(format!("oc-directory-{key}-{index}").as_str())
                || volume
                    .pointer("/Labels/dev.open-compute.container.role")
                    .and_then(Value::as_str)
                    != Some("directory")
            {
                return Err(invalid());
            }
            result.insert(key.to_owned());
        }
    }
    Ok(result)
}

fn accepted_status(operation: &Operation, status: u16) -> bool {
    match operation {
        Operation::Bridge | Operation::Inspect(_) | Operation::ExecInspect(_) => {
            matches!(status, 200 | 404)
        }
        Operation::Create(_) | Operation::ExecCreate(_) => matches!(status, 201 | 404 | 409),
        Operation::Start(_) => matches!(status, 204 | 304 | 404),
        Operation::Stop(_) | Operation::Kill(_) => matches!(status, 204 | 304 | 404 | 409),
        Operation::Wait(_) => matches!(status, 200 | 404),
        Operation::Delete(_) | Operation::ExecResize(_) => matches!(status, 200 | 204 | 404),
        Operation::Archive(_) => {
            matches!(status, 200 | 404)
        }
        Operation::ExecStart(_) => status == 200,
    }
}

fn response(status: u16, bytes: Vec<u8>) -> Result<Response<Body>, PlatformError> {
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Body::from(bytes))
        .map_err(|_| invalid())
}

fn failure(status: StatusCode) -> Response<Body> {
    let mut response = Response::new(Body::from(
        json!({"message":"Container provider operation rejected"}).to_string(),
    ));
    *response.status_mut() = status;
    response
}

async fn read_body(body: Body, maximum: usize) -> Result<bytes::Bytes, PlatformError> {
    tokio::time::timeout(std::time::Duration::from_secs(30), to_bytes(body, maximum))
        .await
        .map_err(|_| invalid())?
        .map_err(|_| invalid())
}
