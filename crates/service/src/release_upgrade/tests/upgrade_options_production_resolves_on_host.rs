use super::*;

#[test]
fn upgrade_options_production_resolves_on_host() {
    let options = UpgradeOptions::production(None, true, true, ServiceScope::User);
    if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        assert_eq!(options.unwrap_err().code(), ErrorCode::ReleaseUnsupported);
        assert_eq!(
            host_release_target().unwrap_err().code(),
            ErrorCode::ReleaseUnsupported
        );
        return;
    }
    let options = options.unwrap();
    assert!(options.binary_path.is_absolute() || options.binary_path.exists());
    assert!(!options.target.is_empty());
    assert_eq!(host_release_target().unwrap(), options.target);
}
