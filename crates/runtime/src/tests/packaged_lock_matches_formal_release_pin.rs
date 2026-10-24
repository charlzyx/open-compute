use super::*;

#[test]
fn packaged_lock_matches_formal_release_pin() {
    let profile = option_env!("OPEN_COMPUTE_BUILD_PROFILE");
    let path = packaged_runtime_lock_path();
    let (lock, bytes) = load_runtime_lock(&path.canonicalize().unwrap()).unwrap();
    let (embedded, embedded_bytes) = crate::embedded_runtime_lock().unwrap();
    assert_eq!(lock, embedded);
    assert_eq!(bytes, embedded_bytes);
    let expected = if profile == Some("linux-x64-glibc228") {
        vec!["linux-x64"]
    } else {
        vec!["darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64"]
    };
    assert_eq!(
        lock.targets.keys().map(String::as_str).collect::<Vec<_>>(),
        expected
    );
}
