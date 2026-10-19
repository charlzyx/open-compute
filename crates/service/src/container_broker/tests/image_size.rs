//! The private image check accepts only the new closed size response.

use super::*;
use crate::container_broker::QualifiedContainerProvider;
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::Response,
    routing::any,
};
use std::sync::Arc;
use tokio::sync::Mutex;

struct Listener(tokio::net::UnixListener);
impl axum::serve::Listener for Listener {
    type Io = tokio::net::UnixStream;
    type Addr = tokio::net::unix::SocketAddr;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        self.0.accept().await.unwrap()
    }
    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.0.local_addr()
    }
}

struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Replies {
    contract: Mutex<ProviderContract>,
    image: Mutex<(u16, Vec<u8>)>,
}

async fn broker(State(reply): State<Arc<Replies>>, request: Request) -> Response<Body> {
    let (status, bytes) = if request.uri().path() == "/open-compute/v1/contract" {
        (
            200,
            serde_json::to_vec(&*reply.contract.lock().await).unwrap(),
        )
    } else {
        assert_eq!(request.method(), Method::POST);
        assert_eq!(request.uri().path(), "/open-compute/v1/images/check");
        reply.image.lock().await.clone()
    };
    Response::builder()
        .status(status)
        .body(Body::from(bytes))
        .unwrap()
}

#[tokio::test]
async fn container_broker_image_size_requires_closed_response_and_explicit_absence() {
    let directory = tempfile::Builder::new()
        .prefix("oc-size-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = std::fs::canonicalize(directory.path())
        .unwrap()
        .join("broker.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    let reply = Arc::new(Replies {
        contract: Mutex::new(contract()),
        image: Mutex::new((200, Vec::new())),
    });
    let app = Router::new()
        .fallback(any(broker))
        .with_state(Arc::clone(&reply));
    let _server = Server(tokio::spawn(async move {
        axum::serve(Listener(listener), app).await.unwrap();
    }));
    let mut config = open_compute_core::config::ContainersConfig {
        broker_socket: socket,
        provider_contract_sha256: contract().sha256().unwrap(),
        max_snapshots: 32,
        max_snapshot_bytes: 8 << 30,
        max_instances: 1,
        resources: resources(),
    };
    let provider = QualifiedContainerProvider::preflight(&config)
        .await
        .unwrap();
    let proof = lease();
    for bytes in [
        Vec::new(),
        b"{}".to_vec(),
        b"{\"size\":-1}".to_vec(),
        b"{\"size\":1.5}".to_vec(),
        b"{\"size\":\"73\"}".to_vec(),
        b"{\"size\":18446744073709551616}".to_vec(),
        b"{\"size\":73,\"unchecked\":true}".to_vec(),
    ] {
        *reply.image.lock().await = (200, bytes);
        assert!(
            provider
                .image_size(&proof.image, &proof.diff_ids)
                .await
                .is_err()
        );
    }
    for size in [0, 173, u64::MAX] {
        *reply.image.lock().await = (200, serde_json::to_vec(&json!({"size":size})).unwrap());
        assert_eq!(
            provider
                .image_size(&proof.image, &proof.diff_ids)
                .await
                .unwrap(),
            Some(size)
        );
    }
    *reply.image.lock().await = (404, Vec::new());
    assert_eq!(
        provider
            .image_size(&proof.image, &proof.diff_ids)
            .await
            .unwrap(),
        None
    );
    *reply.image.lock().await = (500, b"{\"size\":0}".to_vec());
    assert!(
        provider
            .image_size(&proof.image, &proof.diff_ids)
            .await
            .is_err()
    );
    // Even an operator-pinned matching v1 digest cannot admit the superseded wire protocol.
    let mut obsolete = contract();
    obsolete.version = 1;
    config.provider_contract_sha256 = obsolete.sha256().unwrap();
    *reply.contract.lock().await = obsolete;
    assert!(
        QualifiedContainerProvider::preflight(&config)
            .await
            .is_err()
    );
}
