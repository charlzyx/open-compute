//! Immutable secret envelope fingerprints used by Worker descriptors.

use sha2::{Digest as _, Sha256};

/// Hash nonce and ciphertext for a secret descriptor.
#[must_use]
pub fn ciphertext_sha256(nonce: &[u8], ciphertext: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update((nonce.len() as u64).to_be_bytes());
    digest.update(nonce);
    digest.update((ciphertext.len() as u64).to_be_bytes());
    digest.update(ciphertext);
    hex::encode(digest.finalize())
}
