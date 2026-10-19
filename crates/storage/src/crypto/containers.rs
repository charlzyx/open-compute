use super::*;

/// Maximum canonical startup payload, including environment and entrypoint.
pub const MAX_CONTAINER_STARTUP_BYTES: usize = 16 * 1024;

impl SecretCrypto {
    /// Seal startup parameters to one immutable execution and object recreation generation.
    pub fn seal_container_startup(
        &self,
        payload: &SecretBytes,
        instance: InstanceId,
        owner_key: &str,
        engine_key: &str,
    ) -> Result<SecretEnvelope, PlatformError> {
        if payload.expose().is_empty() || payload.expose().len() > MAX_CONTAINER_STARTUP_BYTES {
            return Err(invalid());
        }
        let aad = associated_data(instance, owner_key, engine_key)?;
        let mut nonce_bytes = [0_u8; NONCE_LEN];
        rand::rngs::OsRng
            .try_fill_bytes(&mut nonce_bytes)
            .map_err(|_| invalid())?;
        let ciphertext = self
            .cipher
            .encrypt(
                &XNonce::from(nonce_bytes),
                Payload {
                    msg: payload.expose(),
                    aad: &aad,
                },
            )
            .map_err(|_| invalid())?;
        Ok(SecretEnvelope {
            version: SecretEnvelope::CURRENT_VERSION,
            key_id: self.key_id.clone(),
            algorithm: ALGORITHM.into(),
            nonce: nonce_bytes.to_vec(),
            ciphertext,
        })
    }

    /// Open startup parameters, rejecting identity, envelope, size, or ciphertext mismatch.
    pub fn open_container_startup(
        &self,
        envelope: &SecretEnvelope,
        instance: InstanceId,
        owner_key: &str,
        engine_key: &str,
    ) -> Result<SecretBytes, PlatformError> {
        if envelope.version != SecretEnvelope::CURRENT_VERSION
            || envelope.algorithm != ALGORITHM
            || envelope.nonce.len() != NONCE_LEN
            || envelope.ciphertext.len() <= 16
            || envelope.ciphertext.len() > MAX_CONTAINER_STARTUP_BYTES + 16
        {
            return Err(invalid());
        }
        if envelope.key_id != self.key_id {
            return Err(PlatformError::new(
                ErrorCode::MasterKeyMismatch,
                "Container startup master key mismatch",
            ));
        }
        let aad = associated_data(instance, owner_key, engine_key)?;
        self.cipher
            .decrypt(
                XNonce::from_slice(&envelope.nonce),
                Payload {
                    msg: &envelope.ciphertext,
                    aad: &aad,
                },
            )
            .map(SecretBytes::new)
            .map_err(|_| invalid())
    }
}

fn associated_data(
    instance: InstanceId,
    owner: &str,
    engine: &str,
) -> Result<Vec<u8>, PlatformError> {
    if !is_sha256_fingerprint(owner) || !is_sha256_fingerprint(engine) {
        return Err(invalid());
    }
    let mut aad = Vec::new();
    aad.extend_from_slice(&SECRET_AAD_SCHEMA.to_be_bytes());
    aad.extend_from_slice(b"open-compute/container-startup/v1\0");
    write_framed(&mut aad, instance.as_str().as_bytes())?;
    write_framed(&mut aad, owner.as_bytes())?;
    write_framed(&mut aad, engine.as_bytes())?;
    Ok(aad)
}

fn invalid() -> PlatformError {
    PlatformError::new(
        ErrorCode::ResourceInvariantViolation,
        "Container startup envelope is invalid",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_roundtrip_binds_complete_identity_and_rejects_tampering() {
        let key = SecretBytes::new(vec![7; 32]);
        let fingerprint = crate::master_key::fingerprint_for_test(key.expose());
        let crypto = SecretCrypto::new(&key, &fingerprint).unwrap();
        let instance = InstanceId::generate();
        let owner = "a".repeat(64);
        let engine = "b".repeat(64);
        let payload =
            SecretBytes::new(br#"{"env":{"TOKEN":"sensitive"},"entrypoint":["app"]}"#.to_vec());
        let sealed = crypto
            .seal_container_startup(&payload, instance, &owner, &engine)
            .unwrap();
        assert!(
            !sealed
                .ciphertext
                .windows(9)
                .any(|bytes| bytes == b"sensitive")
        );
        assert_ne!(
            sealed.nonce,
            crypto
                .seal_container_startup(&payload, instance, &owner, &engine)
                .unwrap()
                .nonce
        );
        let reopened = SecretCrypto::new(&key, &fingerprint).unwrap();
        assert_eq!(
            reopened
                .open_container_startup(&sealed, instance, &owner, &engine)
                .unwrap()
                .expose(),
            payload.expose()
        );
        for (instance, owner, engine) in [
            (InstanceId::generate(), owner.clone(), engine.clone()),
            (instance, "c".repeat(64), engine.clone()),
            (instance, owner.clone(), "c".repeat(64)),
            (instance, engine.clone(), owner.clone()),
        ] {
            assert!(
                crypto
                    .open_container_startup(&sealed, instance, &owner, &engine)
                    .is_err()
            );
        }
        for mutation in 0..6 {
            let mut invalid = sealed.clone();
            match mutation {
                0 => invalid.version = 1,
                1 => invalid.algorithm = "OTHER".into(),
                2 => invalid.nonce.pop().map(|_| ()).unwrap(),
                3 => invalid.ciphertext[0] ^= 1,
                4 => invalid.key_id = "f".repeat(64),
                _ => invalid.ciphertext.clear(),
            }
            assert!(
                crypto
                    .open_container_startup(&invalid, instance, &owner, &engine)
                    .is_err()
            );
        }
        let other = SecretCrypto::new(&SecretBytes::new(vec![8; 32]), &fingerprint).unwrap();
        assert!(
            other
                .open_container_startup(&sealed, instance, &owner, &engine)
                .is_err()
        );
    }

    #[test]
    fn startup_enforces_payload_and_context_bounds() {
        let key = SecretBytes::new(vec![7; 32]);
        let fingerprint = crate::master_key::fingerprint_for_test(key.expose());
        let crypto = SecretCrypto::new(&key, &fingerprint).unwrap();
        let instance = InstanceId::generate();
        let owner = "a".repeat(64);
        let engine = "b".repeat(64);
        for size in [0, MAX_CONTAINER_STARTUP_BYTES + 1] {
            assert!(
                crypto
                    .seal_container_startup(
                        &SecretBytes::new(vec![1; size]),
                        instance,
                        &owner,
                        &engine
                    )
                    .is_err()
            );
        }
        let payload = SecretBytes::new(vec![1; MAX_CONTAINER_STARTUP_BYTES]);
        let sealed = crypto
            .seal_container_startup(&payload, instance, &owner, &engine)
            .unwrap();
        assert_eq!(
            crypto
                .open_container_startup(&sealed, instance, &owner, &engine)
                .unwrap()
                .expose(),
            payload.expose()
        );
        for bad in [
            "".to_owned(),
            "A".repeat(64),
            "a".repeat(63),
            "a".repeat(65),
        ] {
            assert!(
                crypto
                    .seal_container_startup(&payload, instance, &bad, &engine)
                    .is_err()
            );
            assert!(
                crypto
                    .open_container_startup(&sealed, instance, &owner, &bad)
                    .is_err()
            );
        }
        let mut oversized = sealed;
        oversized.ciphertext.push(0);
        assert!(
            crypto
                .open_container_startup(&oversized, instance, &owner, &engine)
                .is_err()
        );
    }
}
