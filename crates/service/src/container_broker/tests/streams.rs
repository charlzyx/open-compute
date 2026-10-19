use super::*;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn container_broker_stream_bounds_and_private_loopback_relay() {
    let (mut input, mut feed) = tokio::io::duplex(64);
    let (mut output, mut read) = tokio::io::duplex(64);
    feed.write_all(b"hello").await.unwrap();
    feed.shutdown().await.unwrap();
    super::super::relay::bounded_copy(&mut input, &mut output, 5)
        .await
        .unwrap();
    let mut result = Vec::new();
    read.read_to_end(&mut result).await.unwrap();
    assert_eq!(result, b"hello");
    let (mut input, mut feed) = tokio::io::duplex(64);
    let (mut output, _read) = tokio::io::duplex(64);
    feed.write_all(b"too long").await.unwrap();
    feed.shutdown().await.unwrap();
    assert!(
        super::super::relay::bounded_copy(&mut input, &mut output, 2)
            .await
            .is_err()
    );
    let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = server.local_addr().unwrap().port();
    let fixture = tokio::spawn(async move {
        let (mut stream, _) = server.accept().await.unwrap();
        let mut bytes = [0; 5];
        stream.read_exact(&mut bytes).await.unwrap();
        stream.write_all(&bytes).await.unwrap();
        assert_eq!(stream.read(&mut [0_u8; 1]).await.unwrap(), 0);
    });
    let relay = super::super::relay::Relay::start(
        "127.0.0.1".parse().unwrap(),
        port,
        Arc::new(tokio::sync::Semaphore::new(2)),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(relay.target_port(), port);
    let mut inspect = json!({"NetworkSettings":{"Ports":{}}});
    relay.rewrite(&mut inspect).unwrap();
    let forwarded = super::super::relay::published_port(&inspect).unwrap();
    let mut client = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, forwarded))
        .await
        .unwrap();
    client.write_all(b"hello").await.unwrap();
    let mut result = [0; 5];
    client.read_exact(&mut result).await.unwrap();
    assert_eq!(&result, b"hello");
    drop(relay);
    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            client.read(&mut [0_u8; 1])
        )
        .await
        .unwrap()
        .unwrap(),
        0
    );
    drop(client);
    fixture.await.unwrap();
    for value in [
        json!({}),
        json!({"NetworkSettings":{"Ports":{"39001/tcp":[]}}}),
        json!({"NetworkSettings":{"Ports":{"39001/tcp":[{"HostPort":"0"}]}}}),
        json!({"NetworkSettings":{"Ports":{"39001/tcp":[{"HostPort":"80"},{"HostPort":"81"}]}}}),
    ] {
        assert!(super::super::relay::published_port(&value).is_err());
    }
}

#[tokio::test]
async fn container_broker_import_spool_checks_tar_before_engine_mutation() {
    let directory = tempfile::Builder::new()
        .prefix("oc-cbs-")
        .tempdir_in("/tmp")
        .unwrap();
    let path = std::fs::canonicalize(directory.path()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut archive = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_ustar();
    header.set_size(5);
    header.set_mode(0o644);
    header.set_cksum();
    archive
        .append_data(&mut header, "manifest.json", &b"hello"[..])
        .unwrap();
    let bytes = archive.into_inner().unwrap();
    let (mut file, size) =
        super::super::spool::stage(axum::body::Body::from(bytes.clone()), &path, 1 << 20)
            .await
            .unwrap();
    assert_eq!(size, bytes.len() as u64);
    let mut result = Vec::new();
    file.read_to_end(&mut result).await.unwrap();
    assert_eq!(result, bytes);
    drop(file);
    assert!(
        super::super::spool::stage(axum::body::Body::from(bytes), &path, 10)
            .await
            .is_err()
    );
    assert!(
        super::super::spool::stage(axum::body::Body::from("not tar"), &path, 1 << 20)
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_dir(path).unwrap().count(), 0);
}

#[tokio::test]
async fn container_broker_import_spool_preserves_fragmented_bytes_and_flushes_the_tail() {
    let directory = tempfile::Builder::new()
        .prefix("oc-cbs-")
        .tempdir_in("/tmp")
        .unwrap();
    let path = std::fs::canonicalize(directory.path()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let payload = vec![0x5a; 300 * 1024 + 13];
    let mut archive = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_ustar();
    header.set_size(payload.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    archive
        .append_data(&mut header, "payload", payload.as_slice())
        .unwrap();
    let bytes = archive.into_inner().unwrap();
    let fragments = bytes
        .chunks(127)
        .map(|part| Ok::<_, std::io::Error>(part.to_vec()))
        .collect::<Vec<_>>();
    let body = axum::body::Body::from_stream(futures::stream::iter(fragments));
    let (mut file, size) = super::super::spool::stage_rootfs(body, &path, 1 << 20)
        .await
        .unwrap();
    assert_eq!(
        file.metadata().await.unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(size, bytes.len() as u64);
    let mut saved = Vec::new();
    file.read_to_end(&mut saved).await.unwrap();
    assert_eq!(saved, bytes);
    drop(file);
    assert_eq!(std::fs::read_dir(path).unwrap().count(), 0);
}

#[tokio::test]
async fn container_broker_exec_frame_validation_handles_fragmentation_and_truncation() {
    for (bytes, limit, success) in [
        (vec![1, 0, 0, 0, 0, 0, 0, 3, b'a', b'b', b'c'], 11, true),
        (vec![2, 0, 0, 0, 0, 0, 0, 0], 8, true),
        (vec![3, 0, 0, 0, 0, 0, 0, 0], 8, false),
        (vec![1, 1, 0, 0, 0, 0, 0, 0], 8, false),
        (vec![1, 0, 0, 0, 0, 0, 0, 9], 8, false),
        (vec![1, 0, 0], 8, false),
        (vec![1, 0, 0, 0, 0, 0, 0, 3, b'a'], 11, false),
    ] {
        let (mut input, mut feed) = tokio::io::duplex(1);
        let original = bytes.clone();
        let feeding = tokio::spawn(async move {
            for byte in bytes {
                if feed.write_all(&[byte]).await.is_err() {
                    break;
                }
            }
        });
        let (mut output, mut read) = tokio::io::duplex(64);
        let result = Box::pin(super::super::relay::framed_copy(
            &mut input,
            &mut output,
            limit,
            false,
        ))
        .await;
        assert_eq!(result.is_ok(), success);
        drop(input);
        feeding.await.unwrap();
        drop(output);
        let mut received = Vec::new();
        read.read_to_end(&mut received).await.unwrap();
        if success {
            assert_eq!(received, original);
        }
    }
    let (mut input, mut feed) = tokio::io::duplex(64);
    let (mut output, mut read) = tokio::io::duplex(64);
    feed.write_all(b"raw pty").await.unwrap();
    drop(feed);
    Box::pin(super::super::relay::framed_copy(
        &mut input,
        &mut output,
        7,
        true,
    ))
    .await
    .unwrap();
    let mut bytes = Vec::new();
    read.read_to_end(&mut bytes).await.unwrap();
    assert_eq!(bytes, b"raw pty");
}
