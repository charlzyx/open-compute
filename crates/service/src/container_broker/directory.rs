//! Frozen identity and mount authority for per-lease directory volumes.

use super::{
    contract::{BrokerLease, invalid},
    helper, policy,
};
use open_compute_core::PlatformError;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

pub(super) fn volume_name(lease: &BrokerLease, position: usize) -> String {
    format!("oc-directory-{}-{position}", lease.lease_key)
}

pub(super) fn labels(lease: &BrokerLease, position: usize) -> Result<Value, PlatformError> {
    let directory = lease
        .directory_snapshots
        .get(position)
        .ok_or_else(invalid)?;
    let mut hash = Sha256::new();
    hash.update(b"dev.open-compute.container.directory.v1\0");
    hash.update(directory.mount_point.as_str().as_bytes());
    hash.update([0]);
    if let Some(archive) = &directory.archive {
        hash.update([1]);
        hash.update(archive.sha256.as_bytes());
        hash.update(archive.bytes.to_be_bytes());
    } else {
        hash.update([0]);
    }
    let mut value = policy::labels(lease, "directory");
    value["dev.open-compute.container.directory.sha256"] = hex::encode(hash.finalize()).into();
    value["dev.open-compute.container.directory.index"] = position.to_string().into();
    Ok(value)
}

pub(super) fn mounts(lease: &BrokerLease, helper_volume: &str) -> Value {
    let mut mounts = vec![json!({"Type":"volume","Source":helper_volume,
        "Target":helper::DIRECTORY,"ReadOnly":true,"VolumeOptions":{"NoCopy":true}})];
    // Docker omits ReadOnly for writable mounts in HostConfig inspection.
    mounts.extend(
        lease
            .directory_snapshots
            .iter()
            .enumerate()
            .map(|(position, directory)| {
                json!({"Type":"volume","Source":volume_name(lease, position),
            "Target":directory.mount_point.as_str(),
            "VolumeOptions":{"NoCopy":true}})
            }),
    );
    Value::Array(mounts)
}

pub(super) fn verify_mounts(
    value: &Value,
    lease: &BrokerLease,
    helper_volume: &str,
) -> Result<(), PlatformError> {
    let expected = mounts(lease, helper_volume);
    if value.pointer("/HostConfig/Mounts") != Some(&expected) {
        return Err(invalid());
    }
    let actual = value
        .get("Mounts")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    let expected = expected.as_array().ok_or_else(invalid)?;
    if actual.len() != expected.len() {
        return Err(invalid());
    }
    for mount in expected {
        let target = mount
            .get("Target")
            .and_then(Value::as_str)
            .ok_or_else(invalid)?;
        let mut matches = actual
            .iter()
            .filter(|mounted| mounted.get("Destination").and_then(Value::as_str) == Some(target));
        let mounted = matches.next().ok_or_else(invalid)?;
        if matches.next().is_some()
            || mounted.get("Type").and_then(Value::as_str) != Some("volume")
            || mounted.get("Name") != mount.get("Source")
            || mounted.get("Driver").and_then(Value::as_str) != Some("local")
            || mounted.get("RW").and_then(Value::as_bool)
                != Some(mount.get("ReadOnly").and_then(Value::as_bool) != Some(true))
            || mounted.get("Propagation").and_then(Value::as_str) != Some("")
        {
            return Err(invalid());
        }
    }
    Ok(())
}

pub(super) fn verify_volume(
    value: &Value,
    lease: &BrokerLease,
    position: usize,
) -> Result<(), PlatformError> {
    if value.get("Name").and_then(Value::as_str) != Some(volume_name(lease, position).as_str())
        || value.get("Driver").and_then(Value::as_str) != Some("local")
        || value.get("Scope").and_then(Value::as_str) != Some("local")
        || value
            .get("Options")
            .is_some_and(|options| !options.is_null() && options != &json!({}))
        || value.get("Labels") != Some(&labels(lease, position)?)
    {
        return Err(invalid());
    }
    Ok(())
}
