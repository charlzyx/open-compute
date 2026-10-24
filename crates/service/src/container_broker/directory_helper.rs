//! Closed quota-preparation helper: fixed code, isolated network and indexed directory mounts.

use super::{
    contract::{BrokerConfig, BrokerLease, invalid, opaque},
    directory, helper, policy,
};
use open_compute_core::PlatformError;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::path::{Component, Path};

pub(super) const ROOT: &str = "/open-compute-rootfs";
pub(super) const MOUNTS: &str = "/open-compute-directories";

pub(super) fn name(lease: &BrokerLease) -> String {
    format!("{}-directory-helper", lease.lease_key)
}

pub(super) fn upper_path(root: &str, upper: &str) -> Result<(), PlatformError> {
    for path in [Path::new(root), Path::new(upper)] {
        if !path.is_absolute()
            || path
                .components()
                .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
            || path.to_str()
                != Some(
                    path.components()
                        .collect::<std::path::PathBuf>()
                        .to_str()
                        .ok_or_else(invalid)?,
                )
        {
            return Err(invalid());
        }
    }
    let base = Path::new(root).join("overlay2");
    let relative = Path::new(upper)
        .strip_prefix(&base)
        .map_err(|_| invalid())?;
    let parts: Vec<_> = relative.components().collect();
    if parts.len() != 2
        || parts[0].as_os_str().to_str().is_none_or(|id| !opaque(id))
        || parts[1].as_os_str() != "diff"
    {
        return Err(invalid());
    }
    Ok(())
}

pub(super) fn request(
    config: &BrokerConfig,
    lease: &BrokerLease,
    upper: &str,
) -> Result<Value, PlatformError> {
    let mut value = base_request(config, lease, "directory-helper", true)?;
    // Docker requires slave propagation for a bind below its own data root.
    value["HostConfig"]["Mounts"].as_array_mut().ok_or_else(invalid)?.insert(1,
        json!({"Type":"bind","Source":upper,"Target":ROOT,"ReadOnly":true,"BindOptions":{"Propagation":"rslave"}}));
    Ok(value)
}

pub(super) fn archive_request(
    config: &BrokerConfig,
    lease: &BrokerLease,
) -> Result<Value, PlatformError> {
    // This receiver never starts; Docker extracts only rebased paths into its owned volumes.
    base_request(config, lease, "directory-archive", false)
}

fn base_request(
    config: &BrokerConfig,
    lease: &BrokerLease,
    role: &str,
    readonly: bool,
) -> Result<Value, PlatformError> {
    let mut mounts = vec![
        json!({"Type":"volume","Source":config.exec_helper_volume,"Target":helper::DIRECTORY,"ReadOnly":true,"VolumeOptions":{"NoCopy":true}}),
    ];
    // Docker omits ReadOnly for writable mounts in HostConfig inspection.
    for position in 0..lease.directory_snapshots.len() {
        mounts.push(json!({"Type":"volume","Source":directory::volume_name(lease,position),"Target":format!("{MOUNTS}/{position}"),"VolumeOptions":{"NoCopy":true}}));
    }
    let mut labels = policy::labels(lease, role);
    labels["dev.open-compute.container.directory.plan"] = hex::encode(Sha256::digest(
        serde_json::to_vec(&lease.directory_snapshots).map_err(|_| invalid())?,
    ))
    .into();
    Ok(
        json!({"Image":config.qualification_image,"User":"0","Entrypoint":[helper::PATH],"Cmd":["--open-compute-directory-quota"],"Labels":labels,
        "HostConfig":{"NetworkMode":"none","ReadonlyRootfs":readonly,"CapDrop":["ALL"],"CapAdd":[],
        "NanoCpus":62_500_000,"Memory":67_108_864,"MemorySwap":67_108_864,"PidsLimit":16,
        "Ulimits":[{"Name":"nofile","Soft":64,"Hard":64}],"StorageOpt":{"size":"16777216"},
        "LogConfig":{"Type":"none","Config":{}},"SecurityOpt":["no-new-privileges:true"],"RestartPolicy":{"Name":"no","MaximumRetryCount":0},
        "Privileged":false,"PidMode":"","AutoRemove":false,"Mounts":mounts}}),
    )
}

pub(super) fn verify(
    value: &Value,
    config: &BrokerConfig,
    lease: &BrokerLease,
    upper: &str,
) -> Result<String, PlatformError> {
    verify_expected(value, lease, &request(config, lease, upper)?)
}

pub(super) fn verify_archive(
    value: &Value,
    config: &BrokerConfig,
    lease: &BrokerLease,
) -> Result<String, PlatformError> {
    let id = verify_expected(value, lease, &archive_request(config, lease)?)?;
    if value.pointer("/State/Status").and_then(Value::as_str) != Some("created")
        || value.pointer("/State/Running").and_then(Value::as_bool) != Some(false)
        || value.pointer("/State/Pid").and_then(Value::as_u64) != Some(0)
        || value.pointer("/State/ExitCode").and_then(Value::as_i64) != Some(0)
        || value.pointer("/State/StartedAt").and_then(Value::as_str) != Some("0001-01-01T00:00:00Z")
    {
        return Err(invalid());
    }
    Ok(id)
}

fn verify_expected(
    value: &Value,
    lease: &BrokerLease,
    expected: &Value,
) -> Result<String, PlatformError> {
    let id = value
        .get("Id")
        .and_then(Value::as_str)
        .filter(|id| opaque(id))
        .ok_or_else(invalid)?;
    if value.get("Name").and_then(Value::as_str) != Some(format!("/{}", name(lease)).as_str())
        || value.get("Image") != expected.get("Image")
    {
        return Err(invalid());
    }
    for field in ["User", "Entrypoint", "Cmd"] {
        if value.get("Config").and_then(|config| config.get(field)) != expected.get(field) {
            return Err(invalid());
        }
    }
    for (key, expected) in expected["Labels"].as_object().ok_or_else(invalid)? {
        if value
            .pointer("/Config/Labels")
            .and_then(|labels| labels.get(key))
            != Some(expected)
        {
            return Err(invalid());
        }
    }
    let host = value
        .get("HostConfig")
        .and_then(Value::as_object)
        .ok_or_else(invalid)?;
    for (key, expected) in expected["HostConfig"].as_object().ok_or_else(invalid)? {
        if key == "CapAdd" {
            if host
                .get(key)
                .is_some_and(|value| !value.is_null() && value != &json!([]))
            {
                return Err(invalid());
            }
        } else if host.get(key) != Some(expected) {
            return Err(invalid());
        }
    }
    for key in [
        "Binds",
        "VolumesFrom",
        "Devices",
        "DeviceRequests",
        "PortBindings",
    ] {
        if host
            .get(key)
            .is_some_and(|value| !value.is_null() && value != &json!([]) && value != &json!({}))
        {
            return Err(invalid());
        }
    }
    let mounts = value
        .get("Mounts")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    let expected = expected["HostConfig"]["Mounts"]
        .as_array()
        .ok_or_else(invalid)?;
    if mounts.len() != expected.len() {
        return Err(invalid());
    }
    for mount in expected {
        let mut found = mounts
            .iter()
            .filter(|value| value.get("Destination") == mount.get("Target"));
        let actual = found.next().ok_or_else(invalid)?;
        if found.next().is_some()
            || actual.get("Type") != mount.get("Type")
            || actual.get("RW").and_then(Value::as_bool)
                != Some(mount.get("ReadOnly").and_then(Value::as_bool) != Some(true))
        {
            return Err(invalid());
        }
        if mount["Type"] == "bind" {
            if actual.get("Source") != mount.get("Source")
                || actual.get("Propagation").and_then(Value::as_str) != Some("rslave")
            {
                return Err(invalid());
            }
        } else if actual.get("Name") != mount.get("Source")
            || actual.get("Driver").and_then(Value::as_str) != Some("local")
            || actual.get("Propagation").and_then(Value::as_str) != Some("")
        {
            return Err(invalid());
        }
    }
    Ok(id.to_owned())
}
