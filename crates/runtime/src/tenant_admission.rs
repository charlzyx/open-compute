//! Host-only bounded tenant admission and namespace policy.

use open_compute_core::{ErrorCode, PlatformError};
use std::ffi::OsStr;

const ADMISSION_ENV: &str = "OPEN_COMPUTE_TENANT_ISOLATE_ADMISSION";
const NAMESPACE_ENV: &str = "OPEN_COMPUTE_TENANT_NAMESPACE_COUNT";
const ADMISSION_PLACEHOLDER: &str = "__OPEN_COMPUTE_TENANT_ISOLATE_ADMISSION__";
const NAMESPACE_PLACEHOLDER: &str = "__OPEN_COMPUTE_TENANT_NAMESPACE_COUNT__";

fn number(value: Option<&OsStr>, default: u16, maximum: u16) -> Result<u16, PlatformError> {
    let Some(value) = value else {
        return Ok(default);
    };
    value
        .to_str()
        .and_then(|text| {
            if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            text.parse::<u16>()
                .ok()
                .filter(|limit| (1..=maximum).contains(limit))
        })
        .ok_or_else(|| {
            PlatformError::new(
                ErrorCode::ConfigInvalid,
                "tenant admission policy must contain bounded positive integers",
            )
        })
}

fn parse(
    admission: Option<&OsStr>,
    namespaces: Option<&OsStr>,
) -> Result<(u16, u16), PlatformError> {
    let admission = number(admission, 48, 1536)?;
    let namespaces = number(namespaces, 1, 32)?;
    let per_namespace = if namespaces == 1 { 64 } else { 48 };
    if admission > namespaces * per_namespace {
        return Err(PlatformError::new(
            ErrorCode::ConfigInvalid,
            "tenant admission exceeds the configured namespace capacity",
        ));
    }
    Ok((admission, namespaces))
}

pub(crate) fn render(template: String) -> Result<String, PlatformError> {
    for placeholder in [ADMISSION_PLACEHOLDER, NAMESPACE_PLACEHOLDER] {
        if template.matches(placeholder).count() != 1 {
            return Err(PlatformError::new(
                ErrorCode::ConfigCompileFailed,
                "config template must contain each tenant admission placeholder exactly once",
            ));
        }
    }
    let admission = std::env::var_os(ADMISSION_ENV);
    let namespaces = std::env::var_os(NAMESPACE_ENV);
    let (admission, namespaces) = parse(admission.as_deref(), namespaces.as_deref())?;
    // Rendered policy participates in the compiled-config input digest.
    Ok(template
        .replace(ADMISSION_PLACEHOLDER, &admission.to_string())
        .replace(NAMESPACE_PLACEHOLDER, &namespaces.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_and_explicit_shard_profiles_are_bounded() {
        assert_eq!(parse(None, None).unwrap(), (48, 1));
        for (admission, namespaces, expected) in [
            ("1", "1", (1, 1)),
            ("64", "1", (64, 1)),
            ("96", "2", (96, 2)),
            ("1024", "32", (1024, 32)),
            ("1536", "32", (1536, 32)),
        ] {
            assert_eq!(
                parse(Some(OsStr::new(admission)), Some(OsStr::new(namespaces))).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn malformed_out_of_range_and_inconsistent_values_fail_closed() {
        for value in ["", "0", "1537", "65536", "-1", "+48", " 48", "48 ", "4.8"] {
            assert!(parse(Some(OsStr::new(value)), None).is_err(), "{value}");
        }
        for value in ["0", "33", "", "-1", "1.5"] {
            assert!(parse(None, Some(OsStr::new(value))).is_err());
        }
        assert!(parse(Some(OsStr::new("65")), None).is_err());
        assert!(parse(Some(OsStr::new("97")), Some(OsStr::new("2"))).is_err());
        use std::os::unix::ffi::OsStrExt;
        assert!(parse(Some(OsStr::from_bytes(&[0xff])), None).is_err());
    }

    #[test]
    fn rendering_requires_both_placeholders_and_preserves_other_config() {
        assert!(render("unchanged".to_string()).is_err());
        assert!(render(ADMISSION_PLACEHOLDER.to_string()).is_err());
        assert!(
            render(format!(
                "{ADMISSION_PLACEHOLDER}{NAMESPACE_PLACEHOLDER}{NAMESPACE_PLACEHOLDER}"
            ))
            .is_err()
        );
        let a = std::env::var_os(ADMISSION_ENV);
        let n = std::env::var_os(NAMESPACE_ENV);
        let (a, n) = parse(a.as_deref(), n.as_deref()).unwrap();
        assert_eq!(
            render(format!(
                "before:{ADMISSION_PLACEHOLDER}:{NAMESPACE_PLACEHOLDER}:after"
            ))
            .unwrap(),
            format!("before:{a}:{n}:after")
        );
    }
}
