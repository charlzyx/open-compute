//! Docker wait sends headers before a potentially quiet container lifetime.

use super::*;
use crate::container_broker::engine::{bounded_response, wait_response};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn response(
    content_length: Option<usize>,
    bytes: Vec<u8>,
) -> (
    reqwest::Response,
    Arc<tokio::sync::Notify>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let release = Arc::new(tokio::sync::Notify::new());
    let gate = release.clone();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            request.push(stream.read_u8().await.unwrap());
            assert!(request.len() < 4096);
        }
        let length = content_length
            .map(|length| format!("Content-Length: {length}\r\n"))
            .unwrap_or_default();
        stream
            .write_all(format!("HTTP/1.1 200 OK\r\nConnection: close\r\n{length}\r\n").as_bytes())
            .await
            .unwrap();
        let mut closed = [0_u8; 1];
        tokio::select! {
            () = gate.notified() => {
                let _ = stream.write_all(&bytes).await;
            }
            result = stream.read(&mut closed) => {
                assert_eq!(result.unwrap(), 0);
            }
        }
    });
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://{address}"))
        .send()
        .await
        .unwrap();
    (response, release, server)
}

#[tokio::test]
async fn wait_quiet_body_outlives_short_response_timeout_and_completes() {
    let (mut broker, provider, lease) = exec_transport::running().await;
    let gate = Arc::new(tokio::sync::Notify::new());
    *broker.state.wait_body_gate.lock().await = Some(gate.clone());
    let client = broker.client();
    let endpoint = format!(
        "http://localhost/containers/{}/wait?condition=not-running",
        lease.lease_key
    );
    let mut waiting = tokio::spawn(async move { client.post(endpoint).send().await });
    tokio::time::timeout(Duration::from_secs(5), broker.state.wait_started.notified())
        .await
        .unwrap();

    let (ordinary, _release, server) = response(None, Vec::new()).await;
    let ordinary = tokio::spawn(bounded_response(ordinary, 16 << 20));
    assert!(
        tokio::time::timeout(Duration::from_secs(31), &mut waiting)
            .await
            .is_err()
    );
    assert!(ordinary.await.unwrap().is_err());
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert!(provider.observe(&lease.lease_key).await.unwrap().running);
    gate.notify_one();
    let response = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&response.bytes().await.unwrap()).unwrap(),
        json!({"StatusCode":7})
    );
    provider.terminate(&lease.lease_key).await.unwrap();
    broker.retire().await;
}

#[tokio::test]
async fn wait_quiet_body_is_cancelled_by_broker_shutdown() {
    let (mut broker, _provider, lease) = exec_transport::running().await;
    let gate = Arc::new(tokio::sync::Notify::new());
    *broker.state.wait_body_gate.lock().await = Some(gate.clone());
    let client = broker.client();
    let waiting = tokio::spawn(async move {
        client
            .post(format!(
                "http://localhost/containers/{}/wait?condition=removed",
                lease.lease_key
            ))
            .send()
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), broker.state.wait_started.notified())
        .await
        .unwrap();
    broker.retire().await;
    if let Ok(response) = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap()
    {
        assert_eq!(response.status(), 503);
    }
    gate.notify_one();
}

#[tokio::test]
async fn wait_response_keeps_length_bounds_and_caller_cancellation() {
    for timed in [false, true] {
        for content_length in [Some(5), None] {
            let (response, release, server) = response(content_length, b"12345".to_vec()).await;
            release.notify_one();
            let result = if timed {
                bounded_response(response, 4).await
            } else {
                wait_response(response, 4).await
            };
            assert!(result.is_err());
            tokio::time::timeout(Duration::from_secs(5), server)
                .await
                .unwrap()
                .unwrap();
        }
    }
    let (response, _release, server) = response(None, Vec::new()).await;
    let waiting = tokio::spawn(wait_response(response, 4));
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());
    waiting.abort();
    assert!(waiting.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
}
