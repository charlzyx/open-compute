//! Private, qualified native Container provider configuration.

use open_compute_core::{ErrorCode, PlatformError, config::validate_container_socket};
use std::path::PathBuf;

pub(crate) const PLACEHOLDER: &str = "__OPEN_COMPUTE_CONTAINER_ENGINE__";

/// A qualified Broker endpoint and already materialized immutable sidecar image.
#[derive(Clone)]
pub struct ContainerProviderConfig {
    broker_socket: PathBuf,
    sidecar_image: String,
}

impl std::fmt::Debug for ContainerProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContainerProviderConfig")
            .finish_non_exhaustive()
    }
}

impl ContainerProviderConfig {
    /// Validate a private Unix socket and Docker config digest without performing I/O.
    pub fn new(broker_socket: PathBuf, sidecar_image: String) -> Result<Self, PlatformError> {
        validate_container_socket(&broker_socket)?;
        if broker_socket.to_str().is_none()
            || !sidecar_image.strip_prefix("sha256:").is_some_and(|digest| {
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
        {
            return Err(invalid());
        }
        Ok(Self {
            broker_socket,
            sidecar_image,
        })
    }
}

pub(crate) fn render(
    template: &str,
    provider: Option<&ContainerProviderConfig>,
) -> Result<String, PlatformError> {
    if template.matches(PLACEHOLDER).count() != 1 {
        return Err(PlatformError::new(
            ErrorCode::ConfigCompileFailed,
            "config template must contain the Container provider placeholder exactly once",
        ));
    }
    let value = match provider {
        None => "none = void".to_owned(),
        Some(provider) => {
            let socket = provider.broker_socket.to_str().ok_or_else(invalid)?;
            let socket = serde_json::to_string(&format!("unix:{socket}")).map_err(|_| invalid())?;
            let image = serde_json::to_string(&provider.sidecar_image).map_err(|_| invalid())?;
            format!(
                "localDocker = (socketPath = {socket}, containerEgressInterceptorImage = {image})"
            )
        }
    };
    Ok(template.replace(PLACEHOLDER, &format!("containerEngine = ({value})")))
}

fn invalid() -> PlatformError {
    PlatformError::new(
        ErrorCode::ConfigInvalid,
        "invalid native Container provider configuration",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualified_provider_rendering_is_private_and_fail_closed() {
        let image = format!("sha256:{}", "a".repeat(64));
        let provider =
            ContainerProviderConfig::new("/tmp/broker.sock".into(), image.clone()).unwrap();
        let rendered = render(PLACEHOLDER, Some(&provider)).unwrap();
        assert_eq!(
            rendered,
            format!(
                "containerEngine = (localDocker = (socketPath = \"unix:/tmp/broker.sock\", containerEgressInterceptorImage = \"{image}\"))"
            )
        );
        assert_eq!(
            render(PLACEHOLDER, None).unwrap(),
            "containerEngine = (none = void)"
        );
        assert!(!format!("{provider:?}").contains("broker.sock"));
        for bad in [
            "sha256:latest".into(),
            format!("sha256:{}", "A".repeat(64)),
            "image:latest".into(),
        ] {
            assert!(ContainerProviderConfig::new("/tmp/broker.sock".into(), bad).is_err());
        }
        assert!(ContainerProviderConfig::new("relative.sock".into(), image.clone()).is_err());
        assert!(render("", None).is_err());
        assert!(render(&format!("{PLACEHOLDER}{PLACEHOLDER}"), Some(&provider)).is_err());
        let escaped = ContainerProviderConfig::new("/tmp/broker\".sock".into(), image).unwrap();
        assert!(
            render(PLACEHOLDER, Some(&escaped))
                .unwrap()
                .contains("broker\\\".sock")
        );
        use std::os::unix::ffi::OsStringExt;
        let non_utf8 = std::ffi::OsString::from_vec(b"/tmp/\xff.sock".to_vec());
        assert!(
            ContainerProviderConfig::new(non_utf8.into(), format!("sha256:{}", "b".repeat(64)))
                .is_err()
        );
    }
}
