use crate::error::{AppError, Result};
use aes_gcm::{
    aead::{Aead, KeyInit, Payload},
    Aes256Gcm, Nonce,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hkdf::Hkdf;
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use uuid::Uuid;

const PRODUCT: &str = "sentinel-monitor";
const APPLICATION_VERSION: &str = "0.2.0";
pub(crate) const CREDENTIAL_ENVELOPE_REVISION: u32 = 1;
const KEY_ID: &str = "sentinel-credentials-0.2.0-key-1";
const KEY_DERIVATION_SALT: &[u8] = b"sentinel-monitor/0.2.0/credential-envelope/key/v1";
const KEY_DERIVATION_INFO: &[u8] = b"sentinel-credential-envelope/aes-256-gcm";
const AAD_DOMAIN: &str = "sentinel-monitor/0.2.0/credential-envelope/aad/v1";
const MAX_ENVELOPE_BYTES: usize = 64 * 1024;
const MAX_PLAINTEXT_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialField {
    MainStreamUrl,
    SubStreamUrl,
    Username,
    Password,
}

impl CredentialField {
    pub const fn database_name(self) -> &'static str {
        match self {
            Self::MainStreamUrl => "main_stream_url_enc",
            Self::SubStreamUrl => "sub_stream_url_enc",
            Self::Username => "username_enc",
            Self::Password => "password_enc",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CredentialEnvelope {
    product: String,
    application_version: String,
    envelope_revision: u32,
    key_id: String,
    nonce: String,
    ciphertext: String,
}

#[derive(Clone)]
pub struct SecretBox {
    cipher: Arc<Aes256Gcm>,
}

impl SecretBox {
    pub fn new(master_key: &[u8; 32]) -> Self {
        let mut key = [0_u8; 32];
        Hkdf::<Sha256>::new(Some(KEY_DERIVATION_SALT), master_key)
            .expand(KEY_DERIVATION_INFO, &mut key)
            .expect("32-byte HKDF output is valid");
        Self {
            cipher: Arc::new(Aes256Gcm::new_from_slice(&key).expect("32-byte key")),
        }
    }

    pub fn encrypt(
        &self,
        camera_id: Uuid,
        field: CredentialField,
        plaintext: &str,
    ) -> Result<Vec<u8>> {
        if plaintext.len() > MAX_PLAINTEXT_BYTES {
            return Err(AppError::Validation(
                "摄像头凭据超过当前协议大小限制".into(),
            ));
        }
        let mut nonce_bytes = [0_u8; 12];
        OsRng.fill_bytes(&mut nonce_bytes);
        let aad = credential_aad(camera_id, field);
        let ciphertext = self
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: plaintext.as_bytes(),
                    aad: &aad,
                },
            )
            .map_err(|_| AppError::Internal("credential envelope encryption failed".into()))?;
        let envelope = CredentialEnvelope {
            product: PRODUCT.into(),
            application_version: APPLICATION_VERSION.into(),
            envelope_revision: CREDENTIAL_ENVELOPE_REVISION,
            key_id: KEY_ID.into(),
            nonce: URL_SAFE_NO_PAD.encode(nonce_bytes),
            ciphertext: URL_SAFE_NO_PAD.encode(ciphertext),
        };
        serde_json::to_vec(&envelope)
            .map_err(|_| AppError::Internal("credential envelope serialization failed".into()))
    }

    pub fn decrypt(
        &self,
        camera_id: Uuid,
        field: CredentialField,
        encoded: &[u8],
    ) -> Result<String> {
        if encoded.is_empty() || encoded.len() > MAX_ENVELOPE_BYTES {
            return Err(malformed_envelope());
        }
        let envelope: CredentialEnvelope =
            serde_json::from_slice(encoded).map_err(|_| malformed_envelope())?;
        let canonical = serde_json::to_vec(&envelope).map_err(|_| malformed_envelope())?;
        if canonical != encoded
            || envelope.product != PRODUCT
            || envelope.application_version != APPLICATION_VERSION
            || envelope.envelope_revision != CREDENTIAL_ENVELOPE_REVISION
            || envelope.key_id != KEY_ID
        {
            return Err(malformed_envelope());
        }
        let nonce = decode_canonical_base64(&envelope.nonce)?;
        let nonce: [u8; 12] = nonce.try_into().map_err(|_| malformed_envelope())?;
        let ciphertext = decode_canonical_base64(&envelope.ciphertext)?;
        if ciphertext.len() < 16 || ciphertext.len() > MAX_PLAINTEXT_BYTES + 16 {
            return Err(malformed_envelope());
        }
        let aad = credential_aad(camera_id, field);
        let plaintext = self
            .cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| malformed_envelope())?;
        String::from_utf8(plaintext).map_err(|_| malformed_envelope())
    }
}

fn decode_canonical_base64(encoded: &str) -> Result<Vec<u8>> {
    let decoded = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| malformed_envelope())?;
    if URL_SAFE_NO_PAD.encode(&decoded) != encoded {
        return Err(malformed_envelope());
    }
    Ok(decoded)
}

fn credential_aad(camera_id: Uuid, field: CredentialField) -> Vec<u8> {
    let camera_id = camera_id.hyphenated().to_string();
    let revision = CREDENTIAL_ENVELOPE_REVISION.to_string();
    let mut aad = Vec::new();
    for value in [
        AAD_DOMAIN,
        PRODUCT,
        APPLICATION_VERSION,
        revision.as_str(),
        KEY_ID,
        camera_id.as_str(),
        field.database_name(),
    ] {
        aad.extend_from_slice(&(value.len() as u64).to_be_bytes());
        aad.extend_from_slice(value.as_bytes());
    }
    aad
}

pub(crate) fn credential_contract_sha256() -> String {
    let contract = format!(
        "format=sentinel-credential-envelope-contract-v1\nproduct={PRODUCT}\napplication_version={APPLICATION_VERSION}\nenvelope_revision={CREDENTIAL_ENVELOPE_REVISION}\nkey_id={KEY_ID}\nkey_derivation_salt={}\nkey_derivation_info={}\naad_domain={AAD_DOMAIN}\nfields=main_stream_url_enc,sub_stream_url_enc,username_enc,password_enc\nnonce_bytes=12\ntag_bytes=16\nmax_envelope_bytes={MAX_ENVELOPE_BYTES}\nmax_plaintext_bytes={MAX_PLAINTEXT_BYTES}\n",
        String::from_utf8_lossy(KEY_DERIVATION_SALT),
        String::from_utf8_lossy(KEY_DERIVATION_INFO),
    );
    format!("{:x}", Sha256::digest(contract.as_bytes()))
}

fn malformed_envelope() -> AppError {
    AppError::Internal("credential envelope is not exactly current or authenticated".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;

    fn box_under_test() -> SecretBox {
        SecretBox::new(&[0x42; 32])
    }

    #[test]
    fn credential_identity_and_schema_are_a_single_current_contract() {
        assert_eq!(APPLICATION_VERSION, env!("CARGO_PKG_VERSION"));
        let schema = include_str!("current_schema.sql");
        for field in [
            CredentialField::MainStreamUrl,
            CredentialField::SubStreamUrl,
            CredentialField::Username,
            CredentialField::Password,
        ] {
            assert!(schema.contains(field.database_name()));
        }
        assert!(schema.contains("username_enc BLOB"));
        let camera_table = schema
            .split_once("CREATE TABLE cameras (")
            .and_then(|(_, remainder)| remainder.split_once("\n);"))
            .map(|(table, _)| table)
            .expect("current schema must contain the cameras table");
        assert!(!camera_table
            .lines()
            .any(|line| line.trim_start().starts_with("username ")));
    }

    #[test]
    fn current_envelope_is_canonical_and_context_bound() {
        let secrets = box_under_test();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let encoded = secrets
            .encrypt(first, CredentialField::Username, "camera-user")
            .unwrap();
        let envelope: CredentialEnvelope = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(envelope.product, PRODUCT);
        assert_eq!(envelope.application_version, APPLICATION_VERSION);
        assert_eq!(envelope.envelope_revision, CREDENTIAL_ENVELOPE_REVISION);
        assert_eq!(envelope.key_id, KEY_ID);
        assert_eq!(serde_json::to_vec(&envelope).unwrap(), encoded);
        assert_eq!(
            secrets
                .decrypt(first, CredentialField::Username, &encoded)
                .unwrap(),
            "camera-user"
        );
        assert!(secrets
            .decrypt(second, CredentialField::Username, &encoded)
            .is_err());
        for other_field in [
            CredentialField::MainStreamUrl,
            CredentialField::SubStreamUrl,
            CredentialField::Password,
        ] {
            assert!(secrets.decrypt(first, other_field, &encoded).is_err());
        }
        assert!(SecretBox::new(&[0x99; 32])
            .decrypt(first, CredentialField::Username, &encoded)
            .is_err());

        let mut noncanonical = b" ".to_vec();
        noncanonical.extend_from_slice(&encoded);
        assert!(secrets
            .decrypt(first, CredentialField::Username, &noncanonical)
            .is_err());
    }

    #[test]
    fn malformed_non_envelope_ciphertexts_are_not_accepted() {
        let master_key = [0x42; 32];
        let raw = b"not-a-current-credential-envelope".to_vec();
        let secrets = SecretBox::new(&master_key);
        let camera_id = Uuid::new_v4();
        assert!(secrets
            .decrypt(camera_id, CredentialField::Password, &raw)
            .is_err());
        assert!(secrets
            .decrypt(
                camera_id,
                CredentialField::Password,
                STANDARD.encode(raw).as_bytes()
            )
            .is_err());
    }

    #[test]
    fn metadata_unknown_fields_and_tampering_fail_without_secret_disclosure() {
        let secrets = box_under_test();
        let camera_id = Uuid::new_v4();
        let secret = "rtsp://operator:do-not-log@camera.invalid/main";
        let encoded = secrets
            .encrypt(camera_id, CredentialField::MainStreamUrl, secret)
            .unwrap();
        let envelope: CredentialEnvelope = serde_json::from_slice(&encoded).unwrap();

        let mut cases = Vec::new();
        let mut wrong_product = envelope.clone();
        wrong_product.product = "another-product".into();
        cases.push(serde_json::to_vec(&wrong_product).unwrap());
        let mut wrong_version = envelope.clone();
        wrong_version.application_version = "noncurrent-version".into();
        cases.push(serde_json::to_vec(&wrong_version).unwrap());
        let mut wrong_key = envelope.clone();
        wrong_key.key_id = "unknown-key".into();
        cases.push(serde_json::to_vec(&wrong_key).unwrap());
        let mut wrong_revision = envelope.clone();
        wrong_revision.envelope_revision = 2;
        cases.push(serde_json::to_vec(&wrong_revision).unwrap());
        let mut unknown = serde_json::to_value(&envelope).unwrap();
        unknown["unknown"] = true.into();
        cases.push(serde_json::to_vec(&unknown).unwrap());
        let mut tampered = envelope;
        let mut ciphertext = decode_canonical_base64(&tampered.ciphertext).unwrap();
        ciphertext[0] ^= 1;
        tampered.ciphertext = URL_SAFE_NO_PAD.encode(ciphertext);
        cases.push(serde_json::to_vec(&tampered).unwrap());

        for invalid in cases {
            let error = secrets
                .decrypt(camera_id, CredentialField::MainStreamUrl, &invalid)
                .unwrap_err()
                .to_string();
            assert!(!error.contains(secret));
            assert!(!error.contains(&camera_id.to_string()));
            assert!(!error.contains("operator"));
        }
    }
}
