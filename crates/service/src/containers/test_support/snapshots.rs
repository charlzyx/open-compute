use super::*;
use std::sync::atomic::Ordering;

pub(super) async fn capture(protocol: Arc<LeaseProtocol>, key: String, value: Value) -> Response {
    assert_eq!(
        value.as_object().unwrap().len(),
        if value.get("directory").is_some() {
            3
        } else {
            2
        }
    );
    if let Some(directory) = value.get("directory") {
        assert_eq!(directory, "/app/data");
    }
    assert_eq!(value["startIdentity"], "c".repeat(64));
    assert!(value["maxBytes"].as_u64().unwrap() <= 8 << 20);
    assert!(
        protocol
            .registrations
            .lock()
            .await
            .iter()
            .any(|lease| lease.lease_key == key)
    );
    protocol.snapshot_requests.lock().await.push((key, value));
    if protocol.snapshot_block.load(Ordering::SeqCst) {
        protocol.snapshot_entered.notify_one();
        protocol.snapshot_release.notified().await;
    }
    if protocol.snapshot_fail.load(Ordering::SeqCst) {
        return Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::empty())
            .unwrap();
    }
    let rootfs = protocol.snapshot_rootfs.lock().await.clone();
    Response::builder()
        .header(header::CONTENT_TYPE, "application/x-tar")
        .header(header::CONTENT_LENGTH, rootfs.len())
        .body(Body::from(rootfs))
        .unwrap()
}

pub(crate) fn rootfs(content: &[u8]) -> Vec<u8> {
    let mut archive = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_ustar();
    header.set_size(content.len() as u64);
    header.set_mode(0o640);
    archive
        .append_data(&mut header, "tmp/state", content)
        .unwrap();
    archive.into_inner().unwrap()
}
