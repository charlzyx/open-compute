//! Bounded Unix-only Docker transport and active qualification.

use super::contract::{BrokerConfig, ProviderContract, invalid};
use axum::http::Method;
use futures::StreamExt;
use open_compute_core::PlatformError;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

mod anchor;
pub(super) mod callback;
mod helper;
mod resources;
mod sidecar;
mod snapshot;

#[derive(Clone)]
pub(super) struct Engine {
    client: reqwest::Client,
    anchor: std::sync::Arc<anchor::Anchor>,
}

impl Engine {
    pub(super) async fn new(
        path: &std::path::Path,
        health_path: &'static str,
    ) -> Result<Self, PlatformError> {
        super::socket::validate_engine_socket(path)?;
        let anchor = anchor::Anchor::connect(path, health_path).await?;
        Ok(Self {
            anchor,
            client: reqwest::Client::builder()
                .unix_socket(path.to_owned())
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(5))
                .build()
                .map_err(|_| invalid())?,
        })
    }

    pub(super) async fn ensure_active(&self) -> Result<(), PlatformError> {
        self.anchor.check().await
    }

    pub(super) async fn send(
        &self,
        method: Method,
        path: &str,
        body: Vec<u8>,
        upgrade: bool,
    ) -> Result<reqwest::Response, PlatformError> {
        self.ensure_active().await?;
        let mut request = self
            .client
            .request(method.clone(), format!("http://localhost{path}"))
            .header(
                "Content-Type",
                if method == Method::PUT && path.contains("/archive?") {
                    "application/x-tar"
                } else {
                    "application/json"
                },
            )
            .body(body);
        if upgrade {
            request = request
                .header("Connection", "Upgrade")
                .header("Upgrade", "tcp");
        }
        if path.contains("/wait") || path.contains("/stop") {
            request.send().await.map_err(|_| invalid())
        } else {
            let seconds =
                if path.starts_with("/open-compute/v1/leases/") && path.ends_with("/snapshot") {
                    185
                } else {
                    120
                };
            tokio::time::timeout(Duration::from_secs(seconds), request.send())
                .await
                .map_err(|_| invalid())?
                .map_err(|_| invalid())
        }
    }

    pub(super) async fn json(
        &self,
        method: Method,
        path: &str,
        body: Value,
    ) -> Result<(u16, Value), PlatformError> {
        let response = self
            .send(
                method,
                path,
                if body.is_null() {
                    Vec::new()
                } else {
                    serde_json::to_vec(&body).map_err(|_| invalid())?
                },
                false,
            )
            .await?;
        let status = response.status().as_u16();
        let bytes = bounded_response(response, 1 << 20).await?;
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).map_err(|_| invalid())?
        };
        Ok((status, value))
    }

    pub(super) async fn qualify(
        &self,
        config: &BrokerConfig,
    ) -> Result<ProviderContract, PlatformError> {
        let (status, info) = self.json(Method::GET, "/info", Value::Null).await?;
        if status != 200 {
            return Err(invalid());
        }
        let architecture = match info.get("Architecture").and_then(Value::as_str) {
            Some("x86_64" | "amd64") => "amd64",
            Some("aarch64" | "arm64") => "arm64",
            _ => return Err(invalid()),
        };
        let version = info
            .get("ServerVersion")
            .and_then(Value::as_str)
            .filter(|value| value.len() <= 64)
            .ok_or_else(invalid)?;
        let (api_status, api) = self.json(Method::GET, "/version", Value::Null).await?;
        let security = info
            .get("SecurityOptions")
            .and_then(Value::as_array)
            .ok_or_else(invalid)?;
        let seccomp = security.iter().any(|value| {
            value
                .as_str()
                .is_some_and(|value| value.starts_with("name=seccomp,"))
        });
        let rootless = security.iter().any(|value| {
            value
                .as_str()
                .is_some_and(|value| value.starts_with("name=rootless"))
        });
        let eligible = info.get("OSType").and_then(Value::as_str) == Some("linux")
            && info.get("Driver").and_then(Value::as_str) == Some("overlay2")
            && info.get("CgroupVersion").and_then(Value::as_str) == Some("2")
            && seccomp
            && !rootless
            && api_status == 200
            && supports_stop_signal(&api);
        let (qualified, snapshots) = if eligible && config.sidecar_image.is_some() {
            self.verify_helper_volume(config).await?;
            let (resources, snapshots) = self.probe_resources(config).await?;
            let qualified = resources && self.probe_sidecar(config).await?;
            (qualified, qualified && snapshots)
        } else {
            (false, false)
        };
        Ok(ProviderContract {
            version: ProviderContract::VERSION,
            runtime: "docker".into(),
            engine_version: version.into(),
            architecture: architecture.into(),
            isolation: format!(
                "linux-private-pid-seccomp-cgroup-v2-{}",
                info.get("Driver")
                    .and_then(Value::as_str)
                    .filter(|value| value.len() <= 128)
                    .ok_or_else(invalid)?
            ),
            qualified,
            snapshots,
            sidecar_image: config.sidecar_image.clone(),
            exec_helper_sha256: config.exec_helper_sha256.clone(),
            resources: config.resources,
            max_instances: config.max_instances,
            max_image_bytes: config.max_image_bytes,
            max_stream_bytes: config.max_stream_bytes,
        })
    }

    pub(super) async fn send_archive(
        &self,
        method: Method,
        path: &str,
        mut file: tokio::fs::File,
        range: std::ops::Range<u64>,
    ) -> Result<reqwest::Response, PlatformError> {
        self.ensure_active().await?;
        let size = range
            .end
            .checked_sub(range.start)
            .filter(|size| *size > 0)
            .ok_or_else(invalid)?;
        let metadata = file.metadata().await.map_err(|_| invalid())?;
        let file_size = metadata.len();
        if !metadata.is_file()
            || range.end > file_size
            || (method != Method::POST && method != Method::PUT)
            || (method == Method::POST && (range.start != 0 || range.end != file_size))
        {
            return Err(invalid());
        }
        file.seek(std::io::SeekFrom::Start(range.start))
            .await
            .map_err(|_| invalid())?;
        let stream =
            futures::stream::try_unfold((file, size), move |(mut file, left)| async move {
                if left == 0 {
                    return Ok(None);
                }
                let mut buffer = vec![0_u8; left.min(65_536) as usize];
                let count = file.read(&mut buffer).await?;
                if count == 0 {
                    return Err(std::io::Error::other("archive truncated"));
                }
                if count as u64 == left && file.metadata().await?.len() != file_size {
                    return Err(std::io::Error::other("archive size changed"));
                }
                buffer.truncate(count);
                Ok(Some((buffer, (file, left - count as u64))))
            });
        tokio::time::timeout(
            Duration::from_secs(180),
            self.client
                .request(method, format!("http://localhost{path}"))
                .header("Content-Type", "application/x-tar")
                .header("Content-Length", size)
                .body(reqwest::Body::wrap_stream(stream))
                .send(),
        )
        .await
        .map_err(|_| invalid())?
        .map_err(|_| invalid())
    }
}

pub(super) async fn bounded_response(
    response: reqwest::Response,
    maximum: usize,
) -> Result<Vec<u8>, PlatformError> {
    read_response(response, maximum, Some(Duration::from_secs(30))).await
}

pub(super) async fn wait_response(
    response: reqwest::Response,
    maximum: usize,
) -> Result<Vec<u8>, PlatformError> {
    // Docker flushes wait headers before the container exits or is removed.
    read_response(response, maximum, None).await
}

async fn read_response(
    response: reqwest::Response,
    maximum: usize,
    idle_timeout: Option<Duration>,
) -> Result<Vec<u8>, PlatformError> {
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        return Err(invalid());
    }
    let mut stream = response.bytes_stream();
    let mut result = Vec::new();
    loop {
        let part = if let Some(timeout) = idle_timeout {
            tokio::time::timeout(timeout, stream.next())
                .await
                .map_err(|_| invalid())?
        } else {
            stream.next().await
        };
        let Some(part) = part else { break };
        let part = part.map_err(|_| invalid())?;
        if part.len() > maximum.saturating_sub(result.len()) {
            return Err(invalid());
        }
        result.extend_from_slice(&part);
    }
    Ok(result)
}

/// Docker load may report an error inside an HTTP 200 NDJSON response.
pub(super) fn verify_load_response(bytes: &[u8]) -> Result<(), PlatformError> {
    let mut loaded = false;
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let event: Value = serde_json::from_slice(line).map_err(|_| invalid())?;
        let fields = event.as_object().ok_or_else(invalid)?;
        if fields.contains_key("error") || fields.contains_key("errorDetail") {
            return Err(invalid());
        }
        if let Some(stream) = fields.get("stream").and_then(Value::as_str) {
            for line in stream.lines() {
                if line
                    .strip_prefix("Loaded image ID: ")
                    .is_some_and(super::contract::image_id)
                {
                    loaded = true;
                }
            }
        }
    }
    if !loaded {
        return Err(invalid());
    }
    Ok(())
}

// Engine API 1.42 introduced an explicit stop signal; older daemons ignore this query.
pub(super) fn supports_stop_signal(version: &Value) -> bool {
    version
        .get("ApiVersion")
        .and_then(Value::as_str)
        .is_some_and(|value| {
            value.strip_prefix("1.").is_some_and(|minor| {
                !minor.is_empty()
                    && minor.len() <= 3
                    && minor.bytes().all(|byte| byte.is_ascii_digit())
                    && !minor.starts_with('0')
                    && minor.parse::<u16>().is_ok_and(|minor| minor >= 42)
            })
        })
}
