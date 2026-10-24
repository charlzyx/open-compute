//! Container-internal paths; these never identify paths on the operator host.

use crate::{ErrorCode, PlatformError};
use serde::{Deserialize, Serialize};

/// Canonical absolute path inside a Container filesystem.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ContainerDirectoryPath(String);

impl ContainerDirectoryPath {
    /// Normalize an API path lexically without consulting the host filesystem.
    pub fn parse(path: &str) -> Result<Self, PlatformError> {
        if !path.starts_with('/') || path.len() > 4_096 || path.contains('\0') {
            return Err(invalid());
        }
        let mut parts = Vec::new();
        for part in path.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop().ok_or_else(invalid)?;
                }
                _ => parts.push(part),
            }
        }
        Ok(Self(format!("/{}", parts.join("/"))))
    }

    /// Return the canonical Container path, including its leading slash.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ContainerDirectoryPath {
    type Error = PlatformError;

    fn try_from(path: String) -> Result<Self, Self::Error> {
        let parsed = Self::parse(&path)?;
        // Persisted authority must already be canonical; reads cannot repair it.
        if parsed.0 != path {
            return Err(invalid());
        }
        Ok(parsed)
    }
}

impl From<ContainerDirectoryPath> for String {
    fn from(path: ContainerDirectoryPath) -> Self {
        path.0
    }
}

fn invalid() -> PlatformError {
    PlatformError::new(ErrorCode::ConfigInvalid, "Invalid Container directory path")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_paths_normalize_api_inputs_but_reject_noncanonical_authority() {
        for (input, expected) in [
            ("/", "/"),
            ("/app/data", "/app/data"),
            ("//app/./data/../saved/", "/app/saved"),
            ("/目录/备份", "/目录/备份"),
        ] {
            let parsed = ContainerDirectoryPath::parse(input).unwrap();
            assert_eq!(parsed.as_str(), expected);
            let json = serde_json::to_string(&parsed).unwrap();
            assert_eq!(
                serde_json::from_str::<ContainerDirectoryPath>(&json).unwrap(),
                parsed
            );
            if input != expected {
                assert!(
                    serde_json::from_value::<ContainerDirectoryPath>(serde_json::json!(input))
                        .is_err()
                );
            }
        }
        for input in [
            "",
            "relative",
            "/../outside",
            "/app/../../outside",
            "/nul\0path",
        ] {
            assert!(ContainerDirectoryPath::parse(input).is_err());
        }
        assert!(ContainerDirectoryPath::parse(&format!("/{}", "a".repeat(4_096))).is_err());
    }
}
