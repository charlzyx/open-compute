//! Read-only proof of the operator's immutable helper volume and image bytes.

use super::*;
use crate::container_broker::helper;

impl Engine {
    pub(crate) async fn verify_helper_volume(
        &self,
        config: &BrokerConfig,
    ) -> Result<(), PlatformError> {
        let (status, volume) = self
            .json(
                Method::GET,
                &format!("/volumes/{}", config.exec_helper_volume),
                Value::Null,
            )
            .await?;
        if status != 200 {
            return Err(invalid());
        }
        helper::verify_volume(&volume, config)
    }

    pub(crate) async fn verify_helper_bytes(
        &self,
        container: &str,
        digest: &str,
    ) -> Result<(), PlatformError> {
        let bytes = tokio::time::timeout(Duration::from_secs(10), async {
            let response = self
                .send(
                    Method::GET,
                    &format!("/containers/{container}/archive?path={}", helper::PATH),
                    Vec::new(),
                    false,
                )
                .await?;
            if response.status().as_u16() != 200 {
                return Err(invalid());
            }
            bounded_response(response, helper::MAX_BYTES as usize + 65_536).await
        })
        .await
        .map_err(|_| invalid())??;
        let digest = digest.to_owned();
        tokio::task::spawn_blocking(move || helper::verify_archive(&bytes, &digest))
            .await
            .map_err(|_| invalid())?
    }
}
