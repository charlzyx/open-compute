//! Bounded snapshot transfer over the private, peer-authenticated Broker socket.

use super::QualifiedContainerProvider;
use crate::container_broker::{
    contract::{SnapshotCaptureRequest, invalid, opaque},
    spool,
};
use axum::http::Method;
use open_compute_core::PlatformError;
use std::{path::Path, time::Duration};

impl QualifiedContainerProvider {
    /// Capture one positively observed process without granting arbitrary engine access.
    ///
    /// The returned unnamed file contains a verified root filesystem tar at offset zero.
    /// A failed or cancelled capture may require positive lease cleanup before another start.
    pub async fn capture_snapshot(
        &self,
        lease_key: &str,
        expected_start_identity: &str,
        max_bytes: u64,
        spool_directory: &Path,
    ) -> Result<(std::fs::File, u64), PlatformError> {
        if !self.contract.snapshots
            || !opaque(lease_key)
            || !opaque(expected_start_identity)
            || max_bytes == 0
            || max_bytes > self.contract.max_image_bytes
        {
            return Err(invalid());
        }
        let request = SnapshotCaptureRequest {
            start_identity: expected_start_identity.to_owned(),
            max_bytes,
        };
        tokio::time::timeout(Duration::from_secs(185), async {
            let response = self
                .engine
                .send(
                    Method::POST,
                    &format!("/open-compute/v1/leases/{lease_key}/snapshot"),
                    serde_json::to_vec(&request).map_err(|_| invalid())?,
                    false,
                )
                .await?;
            if response.status().as_u16() != 200
                || response
                    .headers()
                    .get("Content-Type")
                    .is_none_or(|value| value != "application/x-tar")
                || response
                    .content_length()
                    .is_none_or(|size| size == 0 || size > max_bytes)
            {
                return Err(invalid());
            }
            let expected = response.content_length().ok_or_else(invalid)?;
            let (file, size) = spool::stage_rootfs(
                axum::body::Body::from_stream(response.bytes_stream()),
                spool_directory,
                max_bytes,
            )
            .await?;
            if size != expected {
                return Err(invalid());
            }
            Ok((file.into_std().await, size))
        })
        .await
        .map_err(|_| invalid())?
    }
}
