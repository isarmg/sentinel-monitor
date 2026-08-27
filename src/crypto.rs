use crate::error::{AppError, Result};
use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use rand::{rngs::OsRng, RngCore};
use std::sync::Arc;

#[derive(Clone)]
pub struct SecretBox {
    cipher: Arc<Aes256Gcm>,
}

impl SecretBox {
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            cipher: Arc::new(Aes256Gcm::new_from_slice(key).expect("32-byte key")),
        }
    }

    pub fn encrypt(&self, plaintext: &str) -> Result<Vec<u8>> {
        let mut nonce_bytes = [0u8; 12];
        OsRng.fill_bytes(&mut nonce_bytes);
        let ciphertext = self
            .cipher
            .encrypt(Nonce::from_slice(&nonce_bytes), plaintext.as_bytes())
            .map_err(|_| AppError::Internal("credential encryption failed".into()))?;

        let mut encoded = Vec::with_capacity(12 + ciphertext.len());
        encoded.extend_from_slice(&nonce_bytes);
        encoded.extend_from_slice(&ciphertext);
        Ok(encoded)
    }

    pub fn decrypt(&self, encoded: &[u8]) -> Result<String> {
        if encoded.len() < 13 {
            return Err(AppError::Internal(
                "encrypted credential is malformed".into(),
            ));
        }
        let plaintext = self
            .cipher
            .decrypt(Nonce::from_slice(&encoded[..12]), &encoded[12..])
            .map_err(|_| AppError::Internal("credential decryption failed".into()))?;
        String::from_utf8(plaintext)
            .map_err(|_| AppError::Internal("decrypted credential is not UTF-8".into()))
    }
}
