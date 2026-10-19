//! Active proof of the authenticated reverse callback transport.

use super::*;
use crate::container_broker::{BrokerControlCredentials, credentials::CONTROL_HEADER};
use subtle::ConstantTimeEq as _;
use tokio::io::AsyncWriteExt as _;

pub(super) async fn probe(
    client: &reqwest::Client,
    address: std::net::SocketAddr,
    credentials: &BrokerControlCredentials,
) -> Result<bool, PlatformError> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut reverse = tokio::net::TcpStream::connect(address).await.map_err(|_| invalid())?;
        let request = format!("CONNECT /open-compute/callback-channel HTTP/1.1\r\nHost: {address}\r\n{CONTROL_HEADER}: {}\r\n\r\n", credentials.control_token);
        reverse.write_all(request.as_bytes()).await.map_err(|_| invalid())?;
        let headers = read_headers(&mut reverse).await?;
        if headers.lines().next() != Some("HTTP/1.1 200 OK") { return Ok(false); }
        let callback = async {
            let request = read_headers(&mut reverse).await?;
            if !valid_callback(&request, &credentials.callback_token) { return Err(invalid()); }
            reverse.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.map_err(|_| invalid())?;
            Ok(())
        };
        let trigger = async {
            let response = client.post(format!("http://{address}/open-compute/probe-callback"))
                .header(CONTROL_HEADER, &credentials.control_token).send().await.map_err(|_| invalid())?;
            if response.status().as_u16() != 204 { return Err(invalid()); }
            bounded_response(response, 1_024).await?;
            Ok(())
        };
        tokio::try_join!(callback, trigger)?;
        Ok(true)
    }).await.map_err(|_| invalid())?
}

async fn read_headers(stream: &mut tokio::net::TcpStream) -> Result<String, PlatformError> {
    let mut bytes = Vec::new();
    while bytes.len() < 8_192 {
        let mut byte = [0_u8; 1];
        if stream.read(&mut byte).await.map_err(|_| invalid())? == 0 {
            return Err(invalid());
        }
        bytes.push(byte[0]);
        if bytes.ends_with(b"\r\n\r\n") {
            return String::from_utf8(bytes).map_err(|_| invalid());
        }
    }
    Err(invalid())
}

pub(in crate::container_broker) fn valid_callback(request: &str, token: &str) -> bool {
    let mut lines = request.split("\r\n");
    if lines.next() != Some("CONNECT open-compute-probe.invalid:1 HTTP/1.1") {
        return false;
    }
    let mut callback = None;
    let mut protocol = None;
    for line in lines.filter(|line| !line.is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            return false;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("X-Open-Compute-Container-Callback") {
            if callback.replace(value).is_some() {
                return false;
            }
        } else if name.eq_ignore_ascii_case("X-Proto") && protocol.replace(value).is_some() {
            return false;
        }
    }
    protocol == Some("tcp")
        && callback.is_some_and(|value| {
            value.len() == 64 && bool::from(value.as_bytes().ct_eq(token.as_bytes()))
        })
}
