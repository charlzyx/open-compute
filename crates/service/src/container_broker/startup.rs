//! Immutable startup options received only from the daemon's encrypted lease authority.

use super::contract::invalid;
use open_compute_core::PlatformError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// One public application label, excluding the Broker's identity namespace.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerLabel {
    /// Validated Docker label name.
    pub name: String,
    /// User-selected label value; omitted from diagnostics.
    pub value: String,
}

impl std::fmt::Debug for BrokerLabel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrokerLabel")
            .finish_non_exhaustive()
    }
}

/// Frozen startup options; retries cannot replace a lease's command, environment or labels.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrokerStartOptions {
    /// Internet policy enforced by native interception and its sidecar configuration.
    pub enable_internet: bool,
    /// Cloudflare's entrypoint override; an empty list preserves the image defaults.
    pub entrypoint: Option<Vec<String>>,
    /// Exact environment overrides, encoded as unique `NAME=value` pairs.
    pub environment_variables: Vec<String>,
    /// Application labels, separate from trusted Broker labels.
    pub labels: Vec<BrokerLabel>,
}

impl std::fmt::Debug for BrokerStartOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrokerStartOptions")
            .field("enable_internet", &self.enable_internet)
            .field("entrypoint_present", &self.entrypoint.is_some())
            .field("environment_count", &self.environment_variables.len())
            .field("label_count", &self.labels.len())
            .finish_non_exhaustive()
    }
}

impl BrokerStartOptions {
    pub(super) fn entrypoint_override(&self) -> Option<&Vec<String>> {
        self.entrypoint.as_ref().filter(|values| !values.is_empty())
    }

    /// Validate bounded, unique startup values without repairing persisted authority.
    pub fn validate(&self) -> Result<(), PlatformError> {
        let strings = |values: &[String], maximum: usize| {
            values.len() <= maximum
                && values
                    .iter()
                    .all(|value| value.len() <= 32_768 && !value.as_bytes().contains(&0))
        };
        if self
            .entrypoint
            .as_ref()
            .is_some_and(|values| !strings(values, 256))
            || self.labels.len() > 10
        {
            return Err(invalid());
        }
        validate_environment(self.environment_variables.iter().map(String::as_str))?;
        let mut names = BTreeSet::new();
        for label in &self.labels {
            if label.name.is_empty()
                || label.name.len() > 16
                || label.name.bytes().any(|byte| byte < 0x20)
                || label.value.len() > 64
                || label.value.bytes().any(|byte| byte < 0x20)
                || label.name.starts_with("dev.open-compute.")
                || !names.insert(&label.name)
            {
                return Err(invalid());
            }
        }
        Ok(())
    }
}

pub(super) fn validate_environment<'a>(
    variables: impl Iterator<Item = &'a str>,
) -> Result<(), PlatformError> {
    let mut names = BTreeSet::new();
    for variable in variables {
        let (name, _) = variable.split_once('=').ok_or_else(invalid)?;
        // Cloudflare accepts ASCII letters, digits, underscores, dots and dashes.
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
            || variable.len() > 32_768
            || variable.as_bytes().contains(&0)
            || !names.insert(name)
            || names.len() > 512
        {
            return Err(invalid());
        }
    }
    Ok(())
}

/// Docker's Linux image defaults merged with the frozen application overrides.
#[derive(Clone)]
pub(super) struct EffectiveStartup {
    environment: Vec<String>,
    command: Vec<String>,
    entrypoint: Vec<String>,
    user: String,
    exec_user: String,
    durable_object: bool,
    working_dir: String,
    labels: serde_json::Map<String, serde_json::Value>,
}

impl EffectiveStartup {
    pub(super) fn environment(&self) -> &[String] {
        &self.environment
    }

    pub(super) fn prepare_create(&self, body: &[u8], name: &str) -> Result<Vec<u8>, PlatformError> {
        if !self.durable_object || name.ends_with("-proxy") {
            return Ok(body.to_vec());
        }
        let mut value: serde_json::Value = serde_json::from_slice(body).map_err(|_| invalid())?;
        let root = value.as_object_mut().ok_or_else(invalid)?;
        root.insert("User".into(), serde_json::json!(self.user));
        root.insert("Entrypoint".into(), serde_json::json!(self.entrypoint));
        root.insert("Cmd".into(), serde_json::json!(self.command));
        let body = serde_json::to_vec(&value).map_err(|_| invalid())?;
        if body.len() > 16 << 20 {
            return Err(invalid());
        }
        Ok(body)
    }

    pub(super) fn prepare_exec(&self, body: &[u8]) -> Result<Vec<u8>, PlatformError> {
        let mut value = super::policy::validate_exec(body, false)?;
        let root = value.as_object_mut().ok_or_else(invalid)?;
        if let Some(user) = root.get("User").and_then(serde_json::Value::as_str)
            && !user.is_empty()
        {
            let numeric = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
            match user.split_once(':') {
                Some((uid, gid)) if numeric(uid) && numeric(gid) => {}
                // Cloudflare runs a bare numeric UID as root and rejects names.
                None if numeric(user) => {
                    root.insert("User".into(), serde_json::json!("0:0"));
                }
                _ => return Err(invalid()),
            }
        }
        if self.durable_object {
            let target_user = root
                .get("User")
                .and_then(serde_json::Value::as_str)
                .filter(|user| !user.is_empty())
                .unwrap_or(&self.exec_user)
                .to_owned();
            let command = root
                .get_mut("Cmd")
                .and_then(serde_json::Value::as_array_mut)
                .ok_or_else(invalid)?;
            if command.len() != 3
                || command[0].as_str() != Some(super::helper::PATH)
                || command[1].as_str() != Some("--open-compute-exec")
                || !command[2].as_str().is_some_and(super::contract::opaque)
            {
                return Err(invalid());
            }
            command.push(serde_json::Value::String(target_user));
            root.insert("User".into(), serde_json::json!("0:0"));
        }
        // Moby adds HOSTNAME and, for a TTY, TERM before merging the container environment.
        // Bare names in exec overrides remove inherited values; PATH remains available.
        let mut removed = BTreeSet::from(["HOSTNAME", "TERM"]);
        for variable in &self.environment {
            let key = variable.split('=').next().ok_or_else(invalid)?;
            if key == "PATH" {
                continue;
            }
            removed.insert(key);
        }
        let mut environment: Vec<_> = removed
            .into_iter()
            .map(|key| serde_json::Value::String(key.to_owned()))
            .collect();
        if let Some(overrides) = root.get("Env") {
            environment.extend(overrides.as_array().ok_or_else(invalid)?.iter().cloned());
        }
        root.insert("Env".into(), serde_json::Value::Array(environment));
        let body = serde_json::to_vec(&value).map_err(|_| invalid())?;
        if body.len() > 16 << 20 {
            return Err(invalid());
        }
        Ok(body)
    }

    pub(super) fn from_image(
        image: &serde_json::Value,
        lease: &super::contract::BrokerLease,
    ) -> Result<Self, PlatformError> {
        let config = image
            .get("Config")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(invalid)?;
        let defaults = array(config.get("Env"))?;
        let mut environment = lease.startup.environment_variables.clone();
        if environment.is_empty() {
            environment = defaults;
        } else {
            // Moby merges overrides first, then the first image value of each absent key.
            for variable in defaults {
                let key = variable.split('=').next().ok_or_else(invalid)?;
                if !environment
                    .iter()
                    .any(|value| value.split('=').next() == Some(key))
                {
                    environment.push(variable);
                }
            }
        }
        if environment.len() > 1_024 {
            return Err(invalid());
        }
        // Preserve the environment runc executes: the last occurrence of each key wins.
        // Sending this complete list at create prevents Moby from restoring duplicate defaults.
        let mut names = BTreeSet::new();
        environment.reverse();
        environment.retain(|variable| {
            names.insert(
                variable
                    .split_once('=')
                    .map_or(variable.as_str(), |(key, _)| key)
                    .to_owned(),
            )
        });
        environment.reverse();
        let (mut command, mut entrypoint) = match lease.startup.entrypoint_override() {
            Some(entrypoint) => (Vec::new(), entrypoint.clone()),
            None => (array(config.get("Cmd"))?, array(config.get("Entrypoint"))?),
        };
        let mut user = string(config.get("User"))?;
        let exec_user = if user.is_empty() {
            "0:0".to_owned()
        } else {
            user.clone()
        };
        let durable_object = lease.scheduling_policy
            == open_compute_storage::containers::ContainerSchedulingPolicy::DurableObject;
        if durable_object {
            entrypoint.append(&mut command);
            command = entrypoint;
            if command.is_empty() {
                return Err(invalid());
            }
            entrypoint = vec![
                super::helper::PATH.to_owned(),
                "--open-compute-main".to_owned(),
                exec_user.clone(),
            ];
            user = "0:0".to_owned();
        }
        let mut labels = match config.get("Labels") {
            None | Some(serde_json::Value::Null) => serde_json::Map::new(),
            Some(value) => value.as_object().cloned().ok_or_else(invalid)?,
        };
        if labels.len() > 256
            || labels.iter().any(|(name, value)| {
                name.len() > 256
                    || !value.as_str().is_some_and(|value| {
                        value.len() <= 32_768 && !value.as_bytes().contains(&0)
                    })
            })
        {
            return Err(invalid());
        }
        for label in &lease.startup.labels {
            labels.insert(
                label.name.clone(),
                serde_json::Value::String(label.value.clone()),
            );
        }
        labels.extend(
            super::policy::labels(lease, "application")
                .as_object()
                .cloned()
                .ok_or_else(invalid)?,
        );
        Ok(Self {
            environment,
            command,
            entrypoint,
            user,
            exec_user,
            durable_object,
            working_dir: string(config.get("WorkingDir"))?,
            labels,
        })
    }

    pub(super) fn verify(
        &self,
        inspect: &serde_json::Value,
        name: &str,
    ) -> Result<(), PlatformError> {
        if name.ends_with("-proxy") {
            return Ok(());
        }
        let config = inspect
            .get("Config")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(invalid)?;
        if array(config.get("Env"))? != self.environment
            || array(config.get("Cmd"))? != self.command
            || array(config.get("Entrypoint"))? != self.entrypoint
            || string(config.get("User"))? != self.user
            || string(config.get("WorkingDir"))? != self.working_dir
            || config.get("Labels").and_then(serde_json::Value::as_object) != Some(&self.labels)
        {
            return Err(invalid());
        }
        Ok(())
    }
}

fn array(value: Option<&serde_json::Value>) -> Result<Vec<String>, PlatformError> {
    match value {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(value) => {
            let values = value
                .as_array()
                .filter(|values| values.len() <= 1_024)
                .ok_or_else(invalid)?;
            values
                .iter()
                .map(|value| {
                    if value.is_null() {
                        return Err(invalid());
                    }
                    string(Some(value))
                })
                .collect()
        }
    }
}

fn string(value: Option<&serde_json::Value>) -> Result<String, PlatformError> {
    match value {
        None | Some(serde_json::Value::Null) => Ok(String::new()),
        Some(value) => value
            .as_str()
            .filter(|value| value.len() <= 32_768 && !value.as_bytes().contains(&0))
            .map(str::to_owned)
            .ok_or_else(invalid),
    }
}
