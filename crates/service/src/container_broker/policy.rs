//! Closed Docker operation and request policy.

use super::contract::{BrokerLease, image_id, invalid, opaque};
use axum::http::{Method, Uri};
use open_compute_core::PlatformError;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Operation {
    Bridge,
    Create(String),
    Inspect(String),
    Start(String),
    Stop(String),
    Kill(String),
    Wait(String),
    Delete(String),
    Archive(String),
    ExecCreate(String),
    ExecStart(String),
    ExecResize(String),
    ExecInspect(String),
}

impl Operation {
    pub(super) fn identity(&self) -> Option<&str> {
        match self {
            Self::Bridge => None,
            Self::Create(key)
            | Self::Inspect(key)
            | Self::Start(key)
            | Self::Stop(key)
            | Self::Kill(key)
            | Self::Wait(key)
            | Self::Delete(key)
            | Self::Archive(key)
            | Self::ExecCreate(key)
            | Self::ExecStart(key)
            | Self::ExecResize(key)
            | Self::ExecInspect(key) => Some(key),
        }
    }

    pub(super) fn starts_process(&self) -> bool {
        matches!(
            self,
            Self::Create(_) | Self::Start(_) | Self::ExecCreate(_) | Self::ExecStart(_)
        )
    }
}

pub(super) fn operation(method: &Method, uri: &Uri) -> Result<Operation, PlatformError> {
    let path = uri.path();
    if path.len() > 256 || path.contains('%') || path.contains("//") {
        return Err(invalid());
    }
    let mut query = BTreeMap::new();
    for (key, value) in url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes()) {
        if query.insert(key.into_owned(), value.into_owned()).is_some() {
            return Err(invalid());
        }
    }
    if method == Method::GET && path == "/networks/bridge" && query.is_empty() {
        return Ok(Operation::Bridge);
    }
    if method == Method::POST && path == "/containers/create" && query.len() == 1 {
        let name = query
            .get("name")
            .filter(|value| container_name(value))
            .ok_or_else(invalid)?;
        return Ok(Operation::Create(name.clone()));
    }
    let parts: Vec<_> = path.split('/').collect();
    if parts.len() < 3 || !parts[0].is_empty() {
        return Err(invalid());
    }
    let key = parts[2];
    if parts[1] == "containers" && container_name(key) {
        let name = key.to_owned();
        return match (method.as_str(), parts.get(3).copied(), parts.len()) {
            ("GET", Some("json"), 4) if query.is_empty() => Ok(Operation::Inspect(name)),
            ("POST", Some("start"), 4) if query.is_empty() => Ok(Operation::Start(name)),
            ("POST", Some("stop"), 4) if query.is_empty() => Ok(Operation::Stop(name)),
            ("POST", Some("kill"), 4)
                if query.len() == 1
                    && query.get("signal").is_some_and(|value| valid_signal(value)) =>
            {
                Ok(Operation::Kill(name))
            }
            ("POST", Some("wait"), 4)
                if query.is_empty()
                    || (query.len() == 1
                        && query.get("condition").is_some_and(|value| {
                            matches!(value.as_str(), "removed" | "not-running")
                        })) =>
            {
                Ok(Operation::Wait(name))
            }
            ("DELETE", None, 3)
                if query.len() == 1 && query.get("force").is_some_and(|value| value == "true") =>
            {
                Ok(Operation::Delete(name))
            }
            ("PUT", Some("archive"), 4)
                if query.len() == 1
                    && !name.ends_with("-proxy")
                    && query.get("path").is_some_and(|value| value == "/etc") =>
            {
                Ok(Operation::Archive(name))
            }
            ("POST", Some("exec"), 4) if query.is_empty() => Ok(Operation::ExecCreate(name)),
            _ => Err(invalid()),
        };
    }
    if parts[1] == "exec" && opaque(key) && parts.len() == 4 {
        return match (method.as_str(), parts[3]) {
            ("POST", "start") if query.is_empty() => Ok(Operation::ExecStart(key.to_owned())),
            ("GET", "json") if query.is_empty() => Ok(Operation::ExecInspect(key.to_owned())),
            ("POST", "resize")
                if query.len() == 2
                    && ["h", "w"].iter().all(|key| {
                        query
                            .get(*key)
                            .is_some_and(|value| value.parse::<u16>().is_ok())
                    }) =>
            {
                Ok(Operation::ExecResize(key.to_owned()))
            }
            _ => Err(invalid()),
        };
    }
    Err(invalid())
}

fn container_name(value: &str) -> bool {
    opaque(value.strip_suffix("-proxy").unwrap_or(value))
}

fn valid_signal(value: &str) -> bool {
    value
        .parse::<u8>()
        .is_ok_and(|signal| (1..=64).contains(&signal) && value == signal.to_string())
}

pub(super) fn prepare_create(
    body: &[u8],
    name: &str,
    lease: &BrokerLease,
    sidecar_image: &str,
    effective_environment: &[String],
    helper_volume: &str,
) -> Result<Vec<u8>, PlatformError> {
    let mut value: Value = serde_json::from_slice(body).map_err(|_| invalid())?;
    let root = value.as_object_mut().ok_or_else(invalid)?;
    only(
        root,
        &[
            "Image",
            "Cmd",
            "Entrypoint",
            "Env",
            "HostConfig",
            "ExposedPorts",
        ],
    )?;
    let sidecar = name.ends_with("-proxy");
    if root.get("Image").and_then(Value::as_str)
        != Some(if sidecar { sidecar_image } else { &lease.image })
    {
        return Err(invalid());
    }
    if let Some(env) = root.get("Env") {
        strings(env, 512, 32_768)?;
    }
    for key in ["Cmd", "Entrypoint"] {
        if let Some(values) = root.get(key) {
            strings(values, 256, 32_768)?;
        }
    }
    if sidecar {
        let cmd = root
            .get("Cmd")
            .and_then(Value::as_array)
            .ok_or_else(invalid)?;
        let args: Vec<_> = cmd
            .iter()
            .map(|value| value.as_str().ok_or_else(invalid))
            .collect::<Result<_, _>>()?;
        if args.len() != 10 && args.len() != 11
            || args[0] != "--http-egress-port"
            || args[1].parse::<u16>().is_err()
            || args[1] == "0"
            || args[2..5]
                != [
                    "--http-ingress-address",
                    "0.0.0.0:39001",
                    "--docker-gateway-cidr",
                ]
            || !cidr(args[5])
            || args[6..8] != ["--dns-enabled", "--tls-intercept"]
            || args[8..10] != ["--control-auth-file", super::credentials::AUTH_PATH]
            || (args.len() == 11 && args[10] != "--disable-ipv6")
            || root.get("ExposedPorts") != Some(&json!({"39001/tcp":{}}))
            || root.contains_key("Env")
            || root.contains_key("Entrypoint")
        {
            return Err(invalid());
        }
    } else if root.contains_key("ExposedPorts") {
        return Err(invalid());
    }
    if !sidecar {
        lease.startup.validate()?;
        if root
            .get("Env")
            .is_some_and(|value| value != &json!(lease.startup.environment_variables))
            || match lease.startup.entrypoint_override() {
                Some(entrypoint) => {
                    root.get("Entrypoint") != Some(&json!(entrypoint))
                        || root.get("Cmd") != Some(&json!([]))
                }
                None => root.contains_key("Entrypoint") || root.contains_key("Cmd"),
            }
        {
            return Err(invalid());
        }
        root.insert("Env".into(), json!(effective_environment));
    }
    let host = root
        .get_mut("HostConfig")
        .and_then(Value::as_object_mut)
        .ok_or_else(invalid)?;
    if sidecar {
        only(
            host,
            &[
                "NetworkMode",
                "PublishAllPorts",
                "Dns",
                "ExtraHosts",
                "CapAdd",
            ],
        )?;
        if host.get("NetworkMode") != Some(&json!("bridge"))
            || host.get("PublishAllPorts") != Some(&json!(true))
            || host.get("Dns") != Some(&json!(["1.1.1.1", "8.8.8.8"]))
            || host.get("ExtraHosts") != Some(&json!(["host.docker.internal:host-gateway"]))
            || host.get("CapAdd") != Some(&json!(["NET_ADMIN"]))
        {
            return Err(invalid());
        }
    } else {
        only(host, &["NetworkMode", "RestartPolicy", "CapDrop"])?;
        if host.get("NetworkMode") != Some(&json!(format!("container:{}-proxy", lease.lease_key)))
            || host.get("RestartPolicy") != Some(&json!({"Name":"no"}))
            || host.get("CapDrop") != Some(&json!(["NET_RAW"]))
        {
            return Err(invalid());
        }
    }
    let resources = lease.resources;
    host.insert("NanoCpus".into(), json!(resources.cpu_nanos));
    host.insert("Memory".into(), json!(resources.memory_bytes));
    host.insert("MemorySwap".into(), json!(resources.memory_bytes));
    host.insert("PidsLimit".into(), json!(resources.pids));
    host.insert(
        "Ulimits".into(),
        json!([{"Name":"nofile", "Soft":resources.open_files, "Hard":resources.open_files}]),
    );
    host.insert(
        "StorageOpt".into(),
        json!({"size":resources.disk_bytes.to_string()}),
    );
    host.insert("LogConfig".into(), json!({"Type":"none"}));
    if !sidecar {
        host.insert("SecurityOpt".into(), json!(["no-new-privileges:true"]));
        host.insert("CapDrop".into(), json!(["NET_RAW"]));
        host.insert("Tmpfs".into(), run_tmpfs(resources.memory_bytes));
        host.insert(
            "Mounts".into(),
            super::directory::mounts(lease, helper_volume),
        );
    }
    let mut identity = labels(lease, if sidecar { "proxy" } else { "application" });
    if !sidecar {
        let fields = identity.as_object_mut().ok_or_else(invalid)?;
        for label in &lease.startup.labels {
            fields.insert(label.name.clone(), json!(label.value));
        }
    }
    root.insert("Labels".into(), identity);
    let body = serde_json::to_vec(&value).map_err(|_| invalid())?;
    if body.len() > 16 << 20 {
        return Err(invalid());
    }
    Ok(body)
}

pub(super) fn labels(lease: &BrokerLease, role: &str) -> Value {
    json!({"dev.open-compute.container.lease":lease.lease_key,
        "dev.open-compute.container.owner":lease.owner_key,
        "dev.open-compute.container.start":lease.start_identity,
        "dev.open-compute.container.contract":lease.provider_contract_sha256,
        "dev.open-compute.container.manifest":lease.manifest_sha256,
        "dev.open-compute.container.role":role})
}

pub(super) fn validate_exec(body: &[u8], start: bool) -> Result<Value, PlatformError> {
    let value: Value = serde_json::from_slice(body).map_err(|_| invalid())?;
    let root = value.as_object().ok_or_else(invalid)?;
    if start {
        only(root, &["Detach", "Tty", "ConsoleSize"])?;
        if root.get("Detach").is_some_and(|value| !value.is_boolean()) {
            return Err(invalid());
        }
        if let Some(size) = root.get("ConsoleSize") {
            let size = size.as_array().ok_or_else(invalid)?;
            if size.len() != 2
                || size
                    .iter()
                    .any(|value| !value.as_u64().is_some_and(|n| n <= 65_535))
                || size.iter().all(|value| value.as_u64() == Some(0))
            {
                return Err(invalid());
            }
        }
    } else {
        only(
            root,
            &[
                "AttachStdin",
                "AttachStdout",
                "AttachStderr",
                "Tty",
                "Cmd",
                "Env",
                "WorkingDir",
                "User",
            ],
        )?;
        strings(root.get("Cmd").ok_or_else(invalid)?, 256, 32_768)?;
        if root
            .get("Cmd")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
        {
            return Err(invalid());
        }
        if let Some(env) = root.get("Env") {
            strings(env, 512, 32_768)?;
            super::startup::validate_environment(
                env.as_array()
                    .ok_or_else(invalid)?
                    .iter()
                    .filter_map(Value::as_str),
            )?;
        }
        for key in ["WorkingDir", "User"] {
            if root.get(key).is_some_and(|value| {
                !value
                    .as_str()
                    .is_some_and(|s| s.len() <= 4_096 && !s.as_bytes().contains(&0))
            }) {
                return Err(invalid());
            }
        }
    }
    for key in ["AttachStdin", "AttachStdout", "AttachStderr", "Tty"] {
        if root.get(key).is_some_and(|value| !value.is_boolean()) {
            return Err(invalid());
        }
    }
    Ok(value)
}

pub(super) fn verify_inspect(
    value: &Value,
    name: &str,
    lease: &BrokerLease,
    sidecar_image: &str,
    expected_sidecar_id: Option<&str>,
    helper_volume: &str,
) -> Result<(), PlatformError> {
    let role = if name.ends_with("-proxy") {
        "proxy"
    } else {
        "application"
    };
    let actual = value
        .pointer("/Config/Labels")
        .and_then(Value::as_object)
        .ok_or_else(invalid)?;
    let expected = labels(lease, role);
    if expected.as_object().is_none_or(|expected| {
        expected
            .iter()
            .any(|(key, value)| actual.get(key) != Some(value))
    }) || value.get("Image").and_then(Value::as_str)
        != Some(if role == "proxy" {
            sidecar_image
        } else {
            &lease.image
        })
    {
        return Err(invalid());
    }
    let host = value
        .get("HostConfig")
        .and_then(Value::as_object)
        .ok_or_else(invalid)?;
    let resources = lease.resources;
    if role == "application"
        && (host.get("CapDrop") != Some(&json!(["NET_RAW"]))
            || host.get("Tmpfs") != Some(&run_tmpfs(resources.memory_bytes))
            || host.get("SecurityOpt") != Some(&json!(["no-new-privileges:true"]))
            || host
                .get("RestartPolicy")
                .and_then(|value| value.get("Name"))
                .and_then(Value::as_str)
                != Some("no")
            || host
                .get("RestartPolicy")
                .and_then(|value| value.get("MaximumRetryCount"))
                .is_some_and(|value| value != &json!(0)))
    {
        return Err(invalid());
    }
    if role == "application" {
        super::directory::verify_mounts(value, lease, helper_volume)?;
    }
    let cap_add = host.get("CapAdd");
    let expected_network = if role == "proxy" {
        "bridge".to_owned()
    } else {
        let id = expected_sidecar_id
            .filter(|id| opaque(id))
            .ok_or_else(invalid)?;
        format!("container:{id}")
    };
    if (role == "proxy" && cap_add != Some(&json!(["NET_ADMIN"])))
        || (role == "proxy"
            && host
                .get("Tmpfs")
                .is_some_and(|value| !value.is_null() && value != &json!({})))
        || (role == "application"
            && cap_add.is_some_and(|value| !value.is_null() && value != &json!([])))
    {
        return Err(invalid());
    }
    if host.get("NanoCpus").and_then(Value::as_u64) != Some(resources.cpu_nanos)
        || host.get("Memory").and_then(Value::as_u64) != Some(resources.memory_bytes)
        || host.get("MemorySwap").and_then(Value::as_u64) != Some(resources.memory_bytes)
        || host.get("PidsLimit").and_then(Value::as_u64) != Some(u64::from(resources.pids))
        || host.get("StorageOpt") != Some(&json!({"size":resources.disk_bytes.to_string()}))
        || host.get("Ulimits")
            != Some(
                &json!([{"Name":"nofile","Soft":resources.open_files,"Hard":resources.open_files}]),
            )
        || host
            .get("Privileged")
            .is_some_and(|value| value != &json!(false))
        || host.get("PidMode").is_some_and(|value| value != &json!(""))
        || ["Binds", "Devices", "VolumesFrom", "DeviceRequests"]
            .iter()
            .any(|key| {
                host.get(*key).is_some_and(|value| {
                    !value.is_null() && value.as_array().is_none_or(|value| !value.is_empty())
                })
            })
        || host.get("NetworkMode").and_then(Value::as_str) != Some(expected_network.as_str())
        || role == "proxy"
            && (host
                .get("Mounts")
                .is_some_and(|value| !value.is_null() && value != &json!([]))
                || value
                    .get("Mounts")
                    .is_some_and(|value| !value.is_null() && value != &json!([])))
    {
        return Err(invalid());
    }
    Ok(())
}

pub(super) fn run_tmpfs(memory_bytes: u64) -> Value {
    json!({"/run":format!("rw,exec,nosuid,nodev,size={memory_bytes}")})
}

fn only(value: &Map<String, Value>, keys: &[&str]) -> Result<(), PlatformError> {
    if value.keys().any(|key| !keys.contains(&key.as_str())) {
        return Err(invalid());
    }
    Ok(())
}

fn strings(value: &Value, count: usize, bytes: usize) -> Result<(), PlatformError> {
    let values = value.as_array().ok_or_else(invalid)?;
    if values.len() > count
        || values.iter().any(|value| {
            !value
                .as_str()
                .is_some_and(|s| s.len() <= bytes && !s.as_bytes().contains(&0))
        })
    {
        return Err(invalid());
    }
    Ok(())
}

fn cidr(value: &str) -> bool {
    value.split_once('/').is_some_and(|(address, bits)| {
        address.parse::<std::net::IpAddr>().is_ok_and(|ip| {
            bits.parse::<u8>()
                .is_ok_and(|bits| bits <= if ip.is_ipv4() { 32 } else { 128 })
        })
    })
}

pub(super) fn image_matches(
    value: &Value,
    image: &str,
    diff_ids: &[String],
    architecture: &str,
) -> bool {
    value.get("Id").and_then(Value::as_str) == Some(image)
        && value.get("Os").and_then(Value::as_str) == Some("linux")
        && value.get("Architecture").and_then(Value::as_str) == Some(architecture)
        && value.pointer("/Config/Volumes").is_none_or(|volumes| {
            volumes.is_null() || volumes.as_object().is_some_and(Map::is_empty)
        })
        && value.pointer("/RootFS/Type").and_then(Value::as_str) == Some("layers")
        && value
            .pointer("/RootFS/Layers")
            .and_then(Value::as_array)
            .is_some_and(|ids| {
                ids.len() == diff_ids.len()
                    && ids
                        .iter()
                        .zip(diff_ids)
                        .all(|(id, expected)| id.as_str() == Some(expected) && image_id(expected))
            })
}
