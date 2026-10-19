//! Fixed, operator-materialized exec helper input and read-only application mount.

use super::contract::{BrokerConfig, invalid, opaque};
use open_compute_core::PlatformError;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::io::Read as _;

pub(super) const DIRECTORY: &str = "/opt/open-compute/exec";
pub(super) const PATH: &str = "/opt/open-compute/exec/helper";
pub(super) const MAX_BYTES: u64 = 16 << 20;

pub(super) fn volume_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
}

pub(super) fn mount(volume: &str) -> Value {
    json!([{"Type":"volume","Source":volume,"Target":DIRECTORY,"ReadOnly":true,
        "VolumeOptions":{"NoCopy":true}}])
}

pub(super) fn labels(image: &str, digest: &str) -> Value {
    json!({"dev.open-compute.exec-helper.image":image,
        "dev.open-compute.exec-helper.sha256":digest})
}

pub(super) fn verify_volume(value: &Value, config: &BrokerConfig) -> Result<(), PlatformError> {
    let image = config.sidecar_image.as_deref().ok_or_else(invalid)?;
    if value.get("Name").and_then(Value::as_str) != Some(&config.exec_helper_volume)
        || value.get("Driver").and_then(Value::as_str) != Some("local")
        || value.get("Scope").and_then(Value::as_str) != Some("local")
        || value
            .get("Options")
            .is_some_and(|value| !value.is_null() && value != &json!({}))
        || value.get("Labels") != Some(&labels(image, &config.exec_helper_sha256))
    {
        return Err(invalid());
    }
    Ok(())
}

pub(super) fn verify_mounts(value: &Value, volume: &str) -> Result<(), PlatformError> {
    if value.pointer("/HostConfig/Mounts") != Some(&mount(volume)) {
        return Err(invalid());
    }
    let mounts = value
        .get("Mounts")
        .and_then(Value::as_array)
        .filter(|mounts| mounts.len() == 1)
        .ok_or_else(invalid)?;
    let mounted = &mounts[0];
    if mounted.get("Type").and_then(Value::as_str) != Some("volume")
        || mounted.get("Name").and_then(Value::as_str) != Some(volume)
        || mounted.get("Driver").and_then(Value::as_str) != Some("local")
        || mounted.get("Destination").and_then(Value::as_str) != Some(DIRECTORY)
        || mounted.get("RW").and_then(Value::as_bool) != Some(false)
        || mounted.get("Propagation").and_then(Value::as_str) != Some("")
    {
        return Err(invalid());
    }
    Ok(())
}

pub(super) fn verify_archive(bytes: &[u8], digest: &str) -> Result<(), PlatformError> {
    if !opaque(digest) {
        return Err(invalid());
    }
    open_compute_artifacts::verify_container_archive(bytes, MAX_BYTES + 65_536, 8)?;
    let mut archive = tar::Archive::new(bytes);
    let mut entries = archive.entries().map_err(|_| invalid())?;
    let mut entry = entries.next().ok_or_else(invalid)?.map_err(|_| invalid())?;
    let size = entry.size();
    let mode = entry.header().mode().map_err(|_| invalid())?;
    if !entry.header().entry_type().is_file()
        || entry.path().map_err(|_| invalid())?.as_ref() != std::path::Path::new("helper")
        || size == 0
        || size > MAX_BYTES
        || mode & 0o555 != 0o555
        || mode & 0o7000 != 0
    {
        return Err(invalid());
    }
    let mut hash = Sha256::new();
    let mut buffer = [0; 16_384];
    loop {
        let count = entry.read(&mut buffer).map_err(|_| invalid())?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    if hex::encode(hash.finalize()) != digest || entries.next().is_some() {
        return Err(invalid());
    }
    Ok(())
}
