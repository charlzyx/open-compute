use crate::dashboard::{content_type, dashboard_identity, decode_assets_sha256};
use open_compute_core::ErrorCode;
use open_compute_workers::system_worker_sources_sha256;
use sha2::{Digest, Sha256};

#[test]
fn embedded_dashboard_digest_and_content_types_are_strict() {
    assert_eq!(decode_assets_sha256(&"ab".repeat(32)).unwrap(), [0xab; 32]);
    for invalid in ["not-hex".to_owned(), "ab".repeat(31), "ab".repeat(33)] {
        assert_eq!(
            decode_assets_sha256(&invalid).unwrap_err().code(),
            ErrorCode::Internal
        );
    }

    for (path, expected) in [
        ("index.css", "text/css; charset=utf-8"),
        ("index.htm", "text/html; charset=utf-8"),
        ("index.HTML", "text/html; charset=utf-8"),
        ("index.js", "text/javascript; charset=utf-8"),
        ("index.mjs", "text/javascript; charset=utf-8"),
        ("data.json", "application/json; charset=utf-8"),
        ("index.js.map", "application/json; charset=utf-8"),
        ("logo.svg", "image/svg+xml"),
        ("logo.png", "image/png"),
        ("photo.jpg", "image/jpeg"),
        ("photo.jpeg", "image/jpeg"),
        ("image.gif", "image/gif"),
        ("image.webp", "image/webp"),
        ("image.avif", "image/avif"),
        ("favicon.ico", "image/x-icon"),
        ("font.woff", "font/woff"),
        ("font.woff2", "font/woff2"),
        ("robots.txt", "text/plain; charset=utf-8"),
        ("module.wasm", "application/wasm"),
        ("LICENSE", "application/octet-stream"),
        ("archive.bin", "application/octet-stream"),
    ] {
        assert_eq!(content_type(path), expected, "path={path}");
    }
}

#[test]
fn dashboard_identity_mixes_assets_with_the_system_worker_manifest() {
    let assets = [0xab; 32];
    let identity = dashboard_identity(&assets);
    assert_ne!(identity, assets);
    assert_eq!(identity, dashboard_identity(&assets));

    let mut digest = Sha256::new();
    digest.update(b"open-compute/system-dashboard/v2\0");
    digest.update(assets);
    digest.update(system_worker_sources_sha256());
    let expected: [u8; 32] = digest.finalize().into();
    assert_eq!(identity, expected);
    assert_ne!(identity, dashboard_identity(&[0xcd; 32]));
}
