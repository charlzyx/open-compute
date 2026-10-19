//! Shared-namespace loopback relay for trusted workerd sidecar control.

use super::contract::invalid;
use open_compute_core::PlatformError;
use serde_json::{Value, json};
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
};

pub(super) struct Relay {
    target_port: u16,
    local_port: u16,
    task: tokio::task::JoinHandle<()>,
    cancellation: tokio_util::sync::CancellationToken,
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.task.abort();
    }
}

impl Relay {
    pub(super) async fn start(
        host: IpAddr,
        port: u16,
        requests: Arc<Semaphore>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<Self, PlatformError> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|_| invalid())?;
        let local_port = listener.local_addr().map_err(|_| invalid())?.port();
        let cancellation = cancellation.child_token();
        let child_cancel = cancellation.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut client, _)) = listener.accept().await {
                let Ok(permit) = requests.clone().try_acquire_owned() else {
                    continue;
                };
                let cancellation = child_cancel.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let connecting = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        TcpStream::connect(SocketAddr::new(host, port)),
                    );
                    let upstream = tokio::select! {
                        result = connecting => result,
                        () = cancellation.cancelled() => return,
                    };
                    if let Ok(Ok(mut upstream)) = upstream {
                        tokio::select! {
                            _ = tokio::io::copy_bidirectional(&mut client, &mut upstream) => {},
                            () = cancellation.cancelled() => {},
                        }
                    }
                });
            }
        });
        Ok(Self {
            target_port: port,
            local_port,
            task,
            cancellation,
        })
    }

    pub(super) fn target_port(&self) -> u16 {
        self.target_port
    }

    pub(super) fn rewrite(&self, value: &mut Value) -> Result<(), PlatformError> {
        let ports = value
            .pointer_mut("/NetworkSettings/Ports")
            .and_then(Value::as_object_mut)
            .ok_or_else(invalid)?;
        ports.insert(
            "39001/tcp".into(),
            json!([{"HostIp":"127.0.0.1","HostPort":self.local_port.to_string()}]),
        );
        Ok(())
    }
}

pub(super) fn published_port(value: &Value) -> Result<u16, PlatformError> {
    let entries = value
        .pointer("/NetworkSettings/Ports/39001~1tcp")
        .and_then(Value::as_array)
        .filter(|value| !value.is_empty() && value.len() <= 2)
        .ok_or_else(invalid)?;
    let port = entries[0]
        .get("HostPort")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|port| *port != 0)
        .ok_or_else(invalid)?;
    if entries.iter().any(|entry| {
        entry
            .get("HostPort")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<u16>().ok())
            != Some(port)
    }) {
        return Err(invalid());
    }
    Ok(port)
}

pub(super) async fn bounded_copy<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    maximum: u64,
) -> std::io::Result<()> {
    let mut buffer = [0_u8; 16_384];
    let mut transferred = 0_u64;
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return writer.shutdown().await;
        }
        if count as u64 > maximum.saturating_sub(transferred) {
            return Err(std::io::Error::other("Container stream limit exceeded"));
        }
        writer.write_all(&buffer[..count]).await?;
        transferred += count as u64;
    }
}

/// Forward only complete Docker stdout/stderr framing, preserving its binary representation.
pub(super) async fn framed_copy<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    maximum: u64,
    tty: bool,
) -> std::io::Result<()> {
    if tty {
        return bounded_copy(reader, writer, maximum).await;
    }
    let mut left = maximum;
    let mut buffer = [0_u8; 16_384];
    loop {
        let mut header = [0_u8; 8];
        if reader.read(&mut header[..1]).await? == 0 {
            return writer.shutdown().await;
        }
        reader.read_exact(&mut header[1..]).await?;
        let length = u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as u64;
        let total = length + header.len() as u64;
        if !matches!(header[0], 1 | 2) || header[1..4] != [0, 0, 0] || total > left {
            return Err(std::io::Error::other("Container exec framing rejected"));
        }
        left -= total;
        writer.write_all(&header).await?;
        let mut payload_left = length;
        while payload_left > 0 {
            let count = buffer.len().min(payload_left as usize);
            reader.read_exact(&mut buffer[..count]).await?;
            writer.write_all(&buffer[..count]).await?;
            payload_left -= count as u64;
        }
    }
}
