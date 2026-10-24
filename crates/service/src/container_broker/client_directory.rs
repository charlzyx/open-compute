//! Closed daemon-only transport for retained directory archive inputs.

use super::QualifiedContainerProvider;
use crate::container_broker::{BrokerLease, contract::invalid, engine::bounded_response};
use open_compute_core::PlatformError;

impl QualifiedContainerProvider {
    /// Stage the exact frozen directory tar before native create; empty mounts accept no archive.
    /// The Broker verifies the tar, byte count and digest before retaining its anonymous spool.
    pub async fn stage_directory_restore(
        &self,
        lease: &BrokerLease,
        position: usize,
        file: std::fs::File,
    ) -> Result<(), PlatformError> {
        lease.validate(&self.contract)?;
        let expected = lease
            .directory_snapshots
            .get(position)
            .and_then(|directory| directory.archive.as_ref())
            .ok_or_else(invalid)?;
        let response = self
            .engine
            .send_archive(
                axum::http::Method::POST,
                &format!(
                    "/open-compute/v1/leases/{}/directories/{position}",
                    lease.lease_key
                ),
                tokio::fs::File::from_std(file),
                0..expected.bytes,
            )
            .await?;
        if response.status().as_u16() != 204 {
            return Err(invalid());
        }
        let bytes = bounded_response(response, 0).await?;
        if !bytes.is_empty() {
            return Err(invalid());
        }
        Ok(())
    }
}
