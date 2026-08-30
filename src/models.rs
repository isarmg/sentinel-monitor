use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, sqlx::FromRow)]
pub struct UserRecord {
    pub id: Uuid,
    pub email: String,
    pub password_hash: String,
    pub role: String,
    pub active: bool,
    pub session_version: i64,
    pub last_login_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Serialize)]
pub struct UserView {
    pub id: Uuid,
    pub email: String,
    pub role: String,
    pub active: bool,
    pub last_login_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<UserRecord> for UserView {
    fn from(value: UserRecord) -> Self {
        Self {
            id: value.id,
            email: value.email,
            role: value.role,
            active: value.active,
            last_login_at: value.last_login_at,
            created_at: value.created_at,
            updated_at: value.updated_at,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateUserRequest {
    pub email: String,
    pub password: String,
    pub role: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateUserRequest {
    pub role: Option<String>,
    pub password: Option<String>,
    pub active: Option<bool>,
}

#[derive(Clone, sqlx::FromRow)]
pub struct CameraRecord {
    pub id: Uuid,
    pub name: String,
    pub location: String,
    pub main_stream_url_enc: Vec<u8>,
    pub sub_stream_url_enc: Option<Vec<u8>>,
    pub onvif_url: Option<String>,
    pub username: Option<String>,
    pub password_enc: Option<Vec<u8>>,
    pub enabled: bool,
    pub record_enabled: bool,
    pub status: String,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Serialize)]
pub struct CameraView {
    pub id: Uuid,
    pub name: String,
    pub location: String,
    pub has_sub_stream: bool,
    pub onvif_configured: bool,
    pub username: Option<String>,
    pub enabled: bool,
    pub record_enabled: bool,
    pub status: String,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<&CameraRecord> for CameraView {
    fn from(value: &CameraRecord) -> Self {
        Self {
            id: value.id,
            name: value.name.clone(),
            location: value.location.clone(),
            has_sub_stream: value.sub_stream_url_enc.is_some(),
            onvif_configured: value.onvif_url.is_some(),
            username: value.username.clone(),
            enabled: value.enabled,
            record_enabled: value.record_enabled,
            status: value.status.clone(),
            last_seen_at: value.last_seen_at,
            created_at: value.created_at,
            updated_at: value.updated_at,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateCameraRequest {
    pub name: String,
    #[serde(default)]
    pub location: String,
    pub main_stream_url: String,
    pub sub_stream_url: Option<String>,
    pub onvif_url: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub record_enabled: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateCameraRequest {
    pub name: Option<String>,
    pub location: Option<String>,
    pub main_stream_url: Option<String>,
    pub sub_stream_url: Option<String>,
    #[serde(default)]
    pub clear_sub_stream: bool,
    pub onvif_url: Option<String>,
    #[serde(default)]
    pub clear_onvif: bool,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(default)]
    pub clear_password: bool,
    pub enabled: Option<bool>,
    pub record_enabled: Option<bool>,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize)]
pub struct CameraMutationResponse {
    pub camera: CameraView,
    pub media_synced: bool,
    pub warning: Option<String>,
    pub operation_id: String,
    pub operation_state: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamTicketQuery {
    pub profile: Option<String>,
}

#[derive(Serialize)]
pub struct StreamTicket {
    pub profile: String,
    pub whep_url: String,
    pub hls_url: String,
    pub token: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PtzRequest {
    pub action: String,
    pub pan: Option<f64>,
    pub tilt: Option<f64>,
    pub zoom: Option<f64>,
}

#[derive(Clone, Serialize, sqlx::FromRow)]
pub struct EventRecord {
    pub id: Uuid,
    pub camera_id: Option<Uuid>,
    pub kind: String,
    pub severity: String,
    pub message: String,
    pub details: Value,
    pub acknowledged_at: Option<DateTime<Utc>>,
    pub acknowledged_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventQuery {
    pub camera_id: Option<Uuid>,
    pub unacknowledged: Option<bool>,
    pub limit: Option<i64>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct AuditRecord {
    pub id: Uuid,
    pub user_id: Option<Uuid>,
    pub action: String,
    pub entity_type: String,
    pub entity_id: Option<Uuid>,
    pub details: Value,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::de::DeserializeOwned;
    use serde_json::json;

    fn rejects_unknown<T: DeserializeOwned>(value: Value) {
        assert!(serde_json::from_value::<T>(value).is_err());
    }

    #[test]
    fn every_public_request_dto_rejects_unknown_fields() {
        rejects_unknown::<LoginRequest>(json!({
            "email": "user@example.test",
            "password": "a-password",
            "unknown": true
        }));
        rejects_unknown::<CreateUserRequest>(json!({
            "email": "user@example.test",
            "password": "a-password",
            "role": "viewer",
            "unknown": true
        }));
        rejects_unknown::<UpdateUserRequest>(json!({ "unknown": true }));
        rejects_unknown::<CreateCameraRequest>(json!({
            "name": "Camera",
            "main_stream_url": "rtsp://camera.invalid/main",
            "unknown": true
        }));
        rejects_unknown::<UpdateCameraRequest>(json!({ "unknown": true }));
        rejects_unknown::<StreamTicketQuery>(json!({ "unknown": true }));
        rejects_unknown::<PtzRequest>(json!({ "action": "stop", "unknown": true }));
        rejects_unknown::<EventQuery>(json!({ "unknown": true }));
    }

    #[test]
    fn required_public_request_fields_cannot_be_omitted() {
        assert!(serde_json::from_value::<LoginRequest>(json!({
            "email": "user@example.test"
        }))
        .is_err());
        assert!(serde_json::from_value::<CreateUserRequest>(json!({
            "email": "user@example.test",
            "password": "a-password"
        }))
        .is_err());
        assert!(serde_json::from_value::<CreateCameraRequest>(json!({
            "main_stream_url": "rtsp://camera.invalid/main"
        }))
        .is_err());
        assert!(serde_json::from_value::<PtzRequest>(json!({})).is_err());
    }
}
