//! Operator-owned Container provider configuration.

use crate::{ErrorCode, PlatformError, containers::ContainerResources};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

/// Instance admission limits and the independently operated Broker endpoint.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ContainersConfig {
    /// Absolute private Unix socket path; never a Docker engine endpoint.
    pub broker_socket: PathBuf,
    /// Expected digest of the qualified, secret-free provider contract.
    pub provider_contract_sha256: String,
    /// Maximum live reservations, including uncertain and stopping leases.
    pub max_instances: u32,
    /// Maximum retained snapshots, including pending captures and incomplete retirement.
    pub max_snapshots: u32,
    /// Maximum retained or reserved snapshot artifact bytes.
    pub max_snapshot_bytes: u64,
    /// Ceiling applied to each admitted application target.
    pub resources: ContainerResources,
}

impl ContainersConfig {
    /// Validate syntax and bounds without touching the provider or filesystem.
    pub fn validate(&self) -> Result<(), PlatformError> {
        validate_container_socket(&self.broker_socket)?;
        if !valid_digest(&self.provider_contract_sha256)
            || !(1..=1_024).contains(&self.max_instances)
            || !(1..=100_000).contains(&self.max_snapshots)
            || !(1..=i64::MAX as u64).contains(&self.max_snapshot_bytes)
        {
            return Err(invalid());
        }
        self.resources.validate(self.resources)
    }
}

/// Reject relative, ambiguous, or overlong Unix socket paths before bind/connect.
pub fn validate_container_socket(path: &Path) -> Result<(), PlatformError> {
    if !path.is_absolute()
        || path.as_os_str().as_encoded_bytes().len() > 103
        || path.as_os_str().as_encoded_bytes().contains(&0)
        || path.file_name().is_none()
        || path
            .components()
            .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
    {
        return Err(invalid());
    }
    Ok(())
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn invalid() -> PlatformError {
    PlatformError::new(
        ErrorCode::ConfigInvalid,
        "invalid Container provider configuration",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_config_rejects_untrusted_endpoint_and_unbounded_policy() {
        let valid = ContainersConfig {
            broker_socket: PathBuf::from("/tmp/container-broker.sock"),
            provider_contract_sha256: "a".repeat(64),
            max_instances: 4,
            max_snapshots: 32,
            max_snapshot_bytes: 8 << 30,
            resources: ContainerResources {
                cpu_nanos: 62_500_000,
                memory_bytes: 256 << 20,
                disk_bytes: 2_000_000_000,
                pids: 128,
                open_files: 1_024,
            },
        };
        valid.validate().unwrap();
        for path in [
            "relative.sock".to_owned(),
            "/tmp/../socket".to_owned(),
            format!("/{}", "x".repeat(103)),
            "/".to_owned(),
            "/tmp/a\0b".to_owned(),
        ] {
            assert!(validate_container_socket(Path::new(&path)).is_err());
        }
        for maximum in [0, 1_025] {
            let mut config = valid.clone();
            config.max_instances = maximum;
            assert!(config.validate().is_err());
        }
        for maximum in [0, 100_001] {
            let mut config = valid.clone();
            config.max_snapshots = maximum;
            assert!(config.validate().is_err());
        }
        for maximum in [0, u64::MAX] {
            let mut config = valid.clone();
            config.max_snapshot_bytes = maximum;
            assert!(config.validate().is_err());
        }
        for digest in ["A".repeat(64), "a".repeat(63)] {
            let mut config = valid.clone();
            config.provider_contract_sha256 = digest;
            assert!(config.validate().is_err());
        }
        let mut config = valid.clone();
        config.resources.pids = 0;
        assert!(config.validate().is_err());
        let mut value = serde_json::to_value(valid).unwrap();
        value["docker_socket"] = "ignored".into();
        assert!(serde_json::from_value::<ContainersConfig>(value).is_err());
    }
}
