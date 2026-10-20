//! Host-only admission policy for the shared tenant loader namespace.

use open_compute_core::{ErrorCode, PlatformError};
use std::ffi::OsStr;

const ENVIRONMENT_VARIABLE: &str = "OPEN_COMPUTE_TENANT_ISOLATE_ADMISSION";
const PLACEHOLDER: &str = "__OPEN_COMPUTE_TENANT_ISOLATE_ADMISSION__";

fn parse(value: Option<&OsStr>) -> Result<u8, PlatformError> {
    let Some(value) = value else { return Ok(48) };
    let limit = value.to_str().and_then(|text| {
        if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        text.parse::<u8>()
            .ok()
            .filter(|limit| (1..=64).contains(limit))
    });
    limit.ok_or_else(|| {
        PlatformError::new(
            ErrorCode::ConfigInvalid,
            "OPEN_COMPUTE_TENANT_ISOLATE_ADMISSION must be an integer from 1 to 64",
        )
    })
}

pub(crate) fn render(template: String) -> Result<String, PlatformError> {
    if template.matches(PLACEHOLDER).count() != 1 {
        return Err(PlatformError::new(
            ErrorCode::ConfigCompileFailed,
            "config template must contain the tenant admission placeholder exactly once",
        ));
    }
    let value = std::env::var_os(ENVIRONMENT_VARIABLE);
    let limit = parse(value.as_deref())?;
    // Rendered policy participates in the existing compiled-config input digest.
    Ok(template.replace(PLACEHOLDER, &limit.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_and_native_cache_bound_are_valid() {
        assert_eq!(parse(None).unwrap(), 48);
        for (value, expected) in [("1", 1), ("48", 48), ("64", 64)] {
            assert_eq!(parse(Some(OsStr::new(value))).unwrap(), expected);
        }
    }

    #[test]
    fn render_requires_one_placeholder_and_preserves_other_config() {
        assert!(render("unchanged".to_string()).is_err());
        assert!(render(format!("{PLACEHOLDER}{PLACEHOLDER}")).is_err());
        let value = std::env::var_os(ENVIRONMENT_VARIABLE);
        let limit = parse(value.as_deref()).unwrap();
        assert_eq!(
            render(format!("before:{PLACEHOLDER}:after")).unwrap(),
            format!("before:{limit}:after")
        );
    }

    #[test]
    fn malformed_or_out_of_range_values_fail_closed() {
        for value in [
            "", "0", "65", "255", "256", "-1", "+48", " 48", "48 ", "4.8",
        ] {
            assert!(parse(Some(OsStr::new(value))).is_err(), "{value}");
        }
        use std::os::unix::ffi::OsStrExt;
        assert!(parse(Some(OsStr::from_bytes(&[0xff]))).is_err());
    }
}
