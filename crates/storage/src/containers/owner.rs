use super::ContainerLeaseIdentity;
use open_compute_core::InstanceId;
use sha2::{Digest, Sha256};

impl ContainerLeaseIdentity {
    /// Opaque snapshot owner, stable across executions but fenced by object recreation.
    #[must_use]
    pub fn owner_key(self, instance: InstanceId) -> String {
        let mut digest = Sha256::new();
        digest.update(b"open-compute.container-owner.v1\0");
        for value in [
            instance.to_string(),
            self.namespace_resource_id.to_string(),
            self.object_id.to_string(),
        ] {
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value.as_bytes());
        }
        digest.update(self.object_generation.to_be_bytes());
        hex::encode(digest.finalize())
    }
}
