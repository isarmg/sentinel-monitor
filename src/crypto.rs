//! Sentinel-specific credential domain and object binding over Foundation secrets.

use crate::error::{AppError, Result};
use sarmg_secret::{SecretBytes, SecretKey};
use sarmg_secret_envelope::EnvelopeDomain;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use uuid::Uuid;

const PRODUCT: &str = "sentinel-monitor";
const APPLICATION_VERSION: &str = "0.2.0";
pub(crate) const CREDENTIAL_ENVELOPE_REVISION: u32 = 1;
const MAX_ENVELOPE_BYTES: usize = 64 * 1024;
const MAX_PLAINTEXT_BYTES: usize = 16 * 1024;

struct CameraCredentialEnvelope;

impl EnvelopeDomain for CameraCredentialEnvelope {
    const DOMAIN: &'static [u8] = b"sentinel-monitor/camera-credential";
    const REVISION: u16 = CREDENTIAL_ENVELOPE_REVISION as u16;
}

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

#[derive(Clone)]
pub struct SecretBox {
    master: Arc<SecretKey<32>>,
}

impl SecretBox {
    pub fn new(master_key: &[u8; 32]) -> Self {
        Self {
            master: Arc::new(SecretKey::new(*master_key)),
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
        sarmg_secret_envelope::seal::<CameraCredentialEnvelope>(
            &self.master,
            &credential_binding(camera_id, field),
            &SecretBytes::new(plaintext.as_bytes().to_vec()),
        )
        .map_err(|_| AppError::Internal("credential envelope encryption failed".into()))
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
        let plaintext = sarmg_secret_envelope::open::<CameraCredentialEnvelope>(
            &self.master,
            &credential_binding(camera_id, field),
            encoded,
        )
        .map_err(|_| malformed_envelope())?;
        String::from_utf8(plaintext.expose().to_vec()).map_err(|_| malformed_envelope())
    }
}

fn credential_binding(camera_id: Uuid, field: CredentialField) -> Vec<u8> {
    let camera_id = camera_id.hyphenated().to_string();
    let field = field.database_name();
    let mut binding = Vec::with_capacity(camera_id.len() + field.len() + 8);
    for component in [camera_id.as_bytes(), field.as_bytes()] {
        binding.extend_from_slice(&(component.len() as u32).to_be_bytes());
        binding.extend_from_slice(component);
    }
    binding
}

pub(crate) fn credential_contract_sha256() -> String {
    let contract = format!(
        "format=sarmg-secret-envelope\nproduct={PRODUCT}\napplication_version={APPLICATION_VERSION}\nenvelope_revision={CREDENTIAL_ENVELOPE_REVISION}\ndomain={}\nbinding=camera_uuid,field\nfields=main_stream_url_enc,sub_stream_url_enc,username_enc,password_enc\nmax_envelope_bytes={MAX_ENVELOPE_BYTES}\nmax_plaintext_bytes={MAX_PLAINTEXT_BYTES}\n",
        String::from_utf8_lossy(CameraCredentialEnvelope::DOMAIN),
    );
    format!("{:x}", Sha256::digest(contract.as_bytes()))
}

fn malformed_envelope() -> AppError {
    AppError::Internal("credential envelope is not exactly current or authenticated".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn box_under_test() -> SecretBox {
        SecretBox::new(&[0x42; 32])
    }

    #[test]
    fn credential_identity_and_schema_are_a_single_current_contract() {
        assert_eq!(APPLICATION_VERSION, env!("CARGO_PKG_VERSION"));
        let schema = include_str!("../schema/generated/current_schema.sql");
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
    fn current_envelope_is_randomized_and_context_bound() {
        let secrets = box_under_test();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let encoded = secrets
            .encrypt(first, CredentialField::Username, "camera-user")
            .unwrap();
        let another = secrets
            .encrypt(first, CredentialField::Username, "camera-user")
            .unwrap();
        assert_ne!(encoded, another);
        assert_eq!(&encoded[..4], b"SGEV");
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
    }

    #[test]
    fn malformed_and_tampered_values_fail_without_secret_disclosure() {
        let secrets = box_under_test();
        let camera_id = Uuid::new_v4();
        let secret = "rtsp://operator:do-not-log@camera.invalid/main";
        let mut tampered = secrets
            .encrypt(camera_id, CredentialField::MainStreamUrl, secret)
            .unwrap();
        *tampered.last_mut().unwrap() ^= 1;

        for invalid in [b"not-an-envelope".to_vec(), tampered] {
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
