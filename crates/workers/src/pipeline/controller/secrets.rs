//! Freeze secret ciphertext and descriptor identities before version publication.

use super::*;

impl VersionController<'_> {
    pub(super) fn encrypt_secrets(
        &self,
        instance_id: InstanceId,
        worker_id: WorkerId,
        version_id: VersionId,
        secrets: &BTreeMap<String, SecretString>,
    ) -> Result<(BTreeMap<String, StoredVersionSecret>, Vec<SecretDescriptor>), PlatformError> {
        let mut stored = BTreeMap::new();
        let mut descriptors = Vec::with_capacity(secrets.len());
        for (name, value) in secrets {
            let revision_id = Uuid::now_v7().to_string();
            let plaintext = SecretBytes::new(value.expose().as_bytes().to_vec());
            let envelope = self.storage.crypto().encrypt(
                &plaintext,
                instance_id,
                worker_id,
                version_id,
                name,
                &revision_id,
            )?;
            descriptors.push(SecretDescriptor {
                name: name.clone(),
                revision_id: revision_id.clone(),
                ciphertext_sha256: ciphertext_sha256(&envelope.nonce, &envelope.ciphertext),
            });
            stored.insert(
                name.clone(),
                StoredVersionSecret {
                    name: name.clone(),
                    revision_id,
                    envelope,
                },
            );
        }
        Ok((stored, descriptors))
    }
}
