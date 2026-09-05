use crate::{
    config::Config,
    error::{AppError, Result},
    protocol::CONTRACT,
    AppState,
};
use axum::{extract::FromRequestParts, http::request::Parts, response::Response};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use hkdf::Hkdf;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;

const MEDIA_JWT_KEY_SALT: &[u8] = b"sentinel-monitor/0.2.1/media-jwt/signing-key";
const MEDIA_JWT_KEY_INFO: &[u8] = b"sentinel-media-jwt-v2/HS256";

#[derive(Clone)]
pub struct CurrentUser {
    pub id: String,
}

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> std::result::Result<Self, Self::Rejection> {
        let identity = sarmg_admin_axum::authenticate_request(
            &state.administrator,
            &parts.headers,
            &parts.uri,
            &parts.method,
            "sentinel-monitor",
            state.administrator_origin,
        )
        .await
        .map_err(|response| *response)?;
        let id = identity.administrator_id.to_string();
        Ok(Self { id })
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaClaims {
    pub protocol: String,
    pub iss: String,
    pub aud: String,
    pub kind: String,
    pub sub: String,
    pub camera_id: Uuid,
    pub path: String,
    pub actions: Vec<String>,
    pub jti: Uuid,
    pub iat: u64,
    pub nbf: u64,
    pub exp: u64,
}

pub fn issue_media_token(
    user_id: &str,
    camera_id: Uuid,
    path: String,
    actions: Vec<String>,
    config: &Config,
) -> Result<(String, DateTime<Utc>)> {
    let now = Utc::now();
    let expires_at = now
        + ChronoDuration::from_std(config.media_token_ttl)
            .map_err(|_| AppError::Internal("invalid media token duration".into()))?;
    let claims = MediaClaims {
        protocol: CONTRACT.media_jwt_protocol.clone(),
        iss: CONTRACT.media_jwt_issuer.clone(),
        aud: CONTRACT.media_jwt_audience.clone(),
        kind: CONTRACT.media_jwt_kind.clone(),
        sub: user_id.to_owned(),
        camera_id,
        path,
        actions,
        jti: Uuid::new_v4(),
        iat: now.timestamp() as u64,
        nbf: now.timestamp() as u64,
        exp: expires_at.timestamp() as u64,
    };
    let signing_key = media_signing_key(config)?;
    let token = encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(&signing_key),
    )
    .map_err(|error| AppError::Internal(format!("media token failed: {error}")))?;
    Ok((token, expires_at))
}

pub fn decode_media_token(token: &str, config: &Config) -> Result<MediaClaims> {
    let mut validation = Validation::new(Algorithm::HS256);
    validation.leeway = 0;
    validation.validate_nbf = true;
    validation.set_required_spec_claims(&["exp", "nbf", "aud", "iss", "sub"]);
    validation.set_audience(&[&CONTRACT.media_jwt_audience]);
    validation.set_issuer(&[&CONTRACT.media_jwt_issuer]);
    let signing_key = media_signing_key(config)?;
    let claims = decode::<MediaClaims>(token, &DecodingKey::from_secret(&signing_key), &validation)
        .map_err(|_| AppError::Unauthorized)?
        .claims;
    let now = u64::try_from(Utc::now().timestamp()).map_err(|_| AppError::Unauthorized)?;
    if claims.protocol != CONTRACT.media_jwt_protocol
        || claims.iss != CONTRACT.media_jwt_issuer
        || claims.aud != CONTRACT.media_jwt_audience
        || claims.kind != CONTRACT.media_jwt_kind
        || sarmg_admin_core::Identifier::new(claims.sub.clone()).is_err()
        || claims.camera_id.is_nil()
        || claims.jti.is_nil()
        || claims.path.is_empty()
        || claims.path.len() > 512
        || claims.actions.is_empty()
        || claims.actions.len() > 4
        || claims
            .actions
            .iter()
            .any(|action| !matches!(action.as_str(), "read" | "playback"))
        || claims.nbf != claims.iat
        || claims.iat > now
        || claims.exp.checked_sub(claims.iat) != Some(config.media_token_ttl.as_secs())
    {
        return Err(AppError::Unauthorized);
    }
    Ok(claims)
}

fn media_signing_key(config: &Config) -> Result<[u8; 32]> {
    let mut key = [0_u8; 32];
    Hkdf::<Sha256>::new(Some(MEDIA_JWT_KEY_SALT), &config.jwt_secret)
        .expand(MEDIA_JWT_KEY_INFO, &mut key)
        .map_err(|_| AppError::Internal("media signing key derivation failed".into()))?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::MediaClaims;
    use serde_json::{json, Value};
    use uuid::Uuid;

    fn current_claims() -> Value {
        json!({
            "protocol":"sentinel-media-jwt-v2","iss":"sentinel-monitor/0.2.1",
            "aud":"sentinel-mediamtx/1.20.0","kind":"media","sub":Uuid::new_v4(),
            "camera_id":Uuid::new_v4(),"path":"camera/main","actions":["read"],
            "jti":Uuid::new_v4(),"iat":1,"nbf":1,"exp":121
        })
    }

    #[test]
    fn media_claims_reject_unknown_and_missing_fields() {
        let mut unknown = current_claims();
        unknown
            .as_object_mut()
            .unwrap()
            .insert("unknown".into(), json!(true));
        assert!(serde_json::from_value::<MediaClaims>(unknown).is_err());
        let mut missing = current_claims();
        missing.as_object_mut().unwrap().remove("jti");
        assert!(serde_json::from_value::<MediaClaims>(missing).is_err());
    }

    #[test]
    fn media_claims_preserve_opaque_foundation_administrator_ids() {
        let identifier = sarmg_admin_auth::random_token().unwrap();
        let mut value = current_claims();
        value["sub"] = json!(identifier);
        let claims: MediaClaims = serde_json::from_value(value).unwrap();
        assert_eq!(claims.sub, identifier);
        assert!(sarmg_admin_core::Identifier::new(claims.sub).is_ok());
    }
}
