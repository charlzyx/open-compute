//! Immutable Container resource policy and durable lease states.

use crate::{ErrorCode, PlatformError};
use serde::{Deserialize, Serialize};

mod directory;
pub use directory::ContainerDirectoryPath;

/// Provider-enforced limits for one application process and its ephemeral filesystem.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContainerResources {
    /// CPU ceiling in billionths of one virtual CPU, matching Docker `NanoCpus`.
    pub cpu_nanos: u64,
    /// Memory ceiling in bytes.
    pub memory_bytes: u64,
    /// Writable filesystem ceiling in bytes.
    pub disk_bytes: u64,
    /// Maximum concurrent processes, including the main process.
    pub pids: u32,
    /// Maximum open file descriptors per process.
    pub open_files: u32,
}

impl ContainerResources {
    /// Reject zero, unrepresentable, or provider-exceeding resource values.
    pub fn validate(self, ceiling: Self) -> Result<(), PlatformError> {
        if self.cpu_nanos == 0
            || self.memory_bytes == 0
            || self.disk_bytes == 0
            || self.pids == 0
            || self.open_files == 0
            || self.cpu_nanos > ceiling.cpu_nanos
            || self.memory_bytes > ceiling.memory_bytes
            || self.disk_bytes > ceiling.disk_bytes
            || self.pids > ceiling.pids
            || self.open_files > ceiling.open_files
            || self.cpu_nanos > i64::MAX as u64
            || self.memory_bytes > i64::MAX as u64
            || self.disk_bytes > i64::MAX as u64
        {
            return Err(PlatformError::new(
                ErrorCode::ConfigInvalid,
                "Container resources exceed the provider policy",
            ));
        }
        Ok(())
    }
}

/// Closed Cloudflare named CPU, memory and ephemeral disk profiles.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContainerInstanceProfile {
    /// One sixteenth vCPU, 256 MiB memory and 2 GB disk.
    Lite,
    /// One quarter vCPU, 1 GiB memory and 4 GB disk.
    Basic,
    /// One half vCPU, 4 GiB memory and 8 GB disk.
    #[serde(rename = "standard-1")]
    Standard1,
    /// One vCPU, 6 GiB memory and 12 GB disk.
    #[serde(rename = "standard-2")]
    Standard2,
    /// Two vCPU, 8 GiB memory and 16 GB disk.
    #[serde(rename = "standard-3")]
    Standard3,
    /// Four vCPU, 12 GiB memory and 20 GB disk.
    #[serde(rename = "standard-4")]
    Standard4,
}

impl ContainerInstanceProfile {
    /// Decode one exact official profile name, rejecting unknown or alternate spellings.
    pub fn parse(value: &str) -> Result<Self, PlatformError> {
        match value {
            "lite" => Ok(Self::Lite),
            "basic" => Ok(Self::Basic),
            "standard-1" => Ok(Self::Standard1),
            "standard-2" => Ok(Self::Standard2),
            "standard-3" => Ok(Self::Standard3),
            "standard-4" => Ok(Self::Standard4),
            _ => Err(PlatformError::new(
                ErrorCode::ConfigInvalid,
                "invalid Container instance profile",
            )),
        }
    }
    /// Canonical official profile token used by native startup JSON.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lite => "lite",
            Self::Basic => "basic",
            Self::Standard1 => "standard-1",
            Self::Standard2 => "standard-2",
            Self::Standard3 => "standard-3",
            Self::Standard4 => "standard-4",
        }
    }
    /// Apply the operator process limits and reject named resources exceeding its policy.
    pub fn resources(
        self,
        policy: ContainerResources,
    ) -> Result<ContainerResources, PlatformError> {
        let (cpu_nanos, memory_bytes, disk_bytes) = match self {
            Self::Lite => (62_500_000, 256 << 20, 2_000_000_000),
            Self::Basic => (250_000_000, 1 << 30, 4_000_000_000),
            Self::Standard1 => (500_000_000, 4 << 30, 8_000_000_000),
            Self::Standard2 => (1_000_000_000, 6 << 30, 12_000_000_000),
            Self::Standard3 => (2_000_000_000, 8 << 30, 16_000_000_000),
            Self::Standard4 => (4_000_000_000, 12 << 30, 20_000_000_000),
        };
        let resources = ContainerResources {
            cpu_nanos,
            memory_bytes,
            disk_bytes,
            pids: policy.pids,
            open_files: policy.open_files,
        };
        resources.validate(policy)?;
        Ok(resources)
    }
}

/// Persisted observation of one immutable Container execution lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerLeaseState {
    /// Capacity and object identity are reserved before provider I/O.
    Admitting,
    /// The provider may have accepted create or start.
    Starting,
    /// The same lease's main process is observed running.
    Running,
    /// Termination and cleanup have not yet been confirmed.
    Stopping,
    /// Termination, cleanup, and fencing are confirmed; capacity is released.
    Stopped,
    /// Provider state is uncertain; capacity and object identity remain reserved.
    Lost,
}

impl ContainerLeaseState {
    /// Canonical SQLite state token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admitting => "admitting",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Lost => "lost",
        }
    }

    /// Decode persisted authority without repairing an unknown value.
    pub fn parse(value: &str) -> Result<Self, PlatformError> {
        match value {
            "admitting" => Ok(Self::Admitting),
            "starting" => Ok(Self::Starting),
            "running" => Ok(Self::Running),
            "stopping" => Ok(Self::Stopping),
            "stopped" => Ok(Self::Stopped),
            "lost" => Ok(Self::Lost),
            _ => Err(PlatformError::new(
                ErrorCode::ResourceInvariantViolation,
                "invalid Container lease state",
            )),
        }
    }
}

#[cfg(test)]
#[path = "containers_tests.rs"]
mod tests;
