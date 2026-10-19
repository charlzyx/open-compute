//! Serialize control grants across startup generations and retain unknown rotation outcomes.

use super::*;

impl ContainerService {
    pub(crate) async fn credentials_for_generation(
        &self,
        key: &str,
        auth: &GenerationAuthRegistry,
        credential: &GenerationCredential,
    ) -> Result<crate::container_broker::BrokerControlCredentials, PlatformError> {
        let mut pending = self.credential_rotation.lock().await;
        if pending.contains(key) || auth.with_current(credential, || ()).is_none() {
            return Err(unavailable());
        }
        let mut hash = Sha256::new();
        hash.update(b"dev.open-compute.container.control-generation.v1\0");
        hash.update(credential.expose().as_bytes());
        // Set before I/O. Cancellation or an unknown response keeps later rotation fenced.
        pending.insert(key.to_owned());
        let credentials = self
            .provider
            .lease_credentials(key, &hex::encode(hash.finalize()))
            .await?;
        pending.remove(key);
        auth.with_current(credential, || credentials)
            .ok_or_else(unavailable)
    }
}
