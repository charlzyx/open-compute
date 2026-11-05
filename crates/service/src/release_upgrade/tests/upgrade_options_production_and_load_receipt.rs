use super::*;

#[test]
fn upgrade_options_production_and_load_receipt() {
    let options = UpgradeOptions::production(None, true, true, ServiceScope::User);
    if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        assert_eq!(options.unwrap_err().code(), ErrorCode::ReleaseUnsupported);
        return;
    }
    let options = options.unwrap();
    assert!(options.binary_path.is_absolute() || options.binary_path.exists());
    assert!(options.dry_run);
    assert!(options.no_restart);
    assert_eq!(
        options.receipt_path,
        receipt_path_in(
            InstanceRegistry::production()
                .unwrap()
                .root_for(ServiceScope::User)
        )
    );
    let _ = LiveReleaseHttp::new().unwrap();
}
