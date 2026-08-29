use crate::{
    config::Config,
    error::{AppError, Result},
    models::{UserRecord, UserView},
    AppState,
};
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use axum::{extract::FromRequestParts, http::request::Parts};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const SESSION_COOKIE: &str = "monitor_session";

#[derive(Clone)]
pub struct CurrentUser {
    pub id: Uuid,
    pub email: String,
    pub role: String,
}

impl CurrentUser {
    pub fn require_admin(&self) -> Result<()> {
        if self.role == "admin" {
            Ok(())
        } else {
            Err(AppError::Forbidden)
        }
    }

    pub fn require_operator(&self) -> Result<()> {
        if self.role == "admin" || self.role == "operator" {
            Ok(())
        } else {
            Err(AppError::Forbidden)
        }
    }

    pub fn view(&self) -> UserView {
        UserView {
            id: self.id,
            email: self.email.clone(),
            role: self.role.clone(),
            active: true,
            last_login_at: None,
            created_at: Utc::now(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct SessionClaims {
    sub: String,
    kind: String,
    iat: usize,
    exp: usize,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct MediaClaims {
    pub sub: String,
    pub camera_id: Uuid,
    pub path: String,
    pub actions: Vec<String>,
    pub kind: String,
    pub iat: usize,
    pub exp: usize,
}

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self> {
        let token = bearer_token(&parts.headers)
            .or_else(|| cookie_token(&parts.headers))
            .ok_or(AppError::Unauthorized)?;
        let user_id = decode_session(&token, &state.config)?;

        let user = sqlx::query_as::<_, UserRecord>(
            "SELECT id, email, password_hash, role, active, last_login_at, created_at, updated_at \
             FROM users WHERE id = ?",
        )
        .bind(user_id)
        .fetch_optional(&state.pool)
        .await?
        .filter(|user| user.active)
        .ok_or(AppError::Unauthorized)?;

        Ok(Self {
            id: user.id,
            email: user.email,
            role: user.role,
        })
    }
}

pub fn hash_password(password: &str) -> Result<String> {
    if password.len() < 12 {
        return Err(AppError::Validation("密码至少需要12个字符".into()));
    }
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| AppError::Internal(format!("password hash failed: {error}")))
}

pub fn verify_password(password: &str, encoded: &str) -> bool {
    let Ok(hash) = PasswordHash::new(encoded) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &hash)
        .is_ok()
}

pub fn issue_session(user_id: Uuid, config: &Config) -> Result<String> {
    let now = Utc::now().timestamp() as usize;
    let claims = SessionClaims {
        sub: user_id.to_string(),
        kind: "session".into(),
        iat: now,
        exp: now + config.session_ttl.as_secs() as usize,
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(&config.jwt_secret),
    )
    .map_err(|error| AppError::Internal(format!("session token failed: {error}")))
}

pub fn issue_media_token(
    user_id: Uuid,
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
        sub: user_id.to_string(),
        camera_id,
        path,
        actions,
        kind: "media".into(),
        iat: now.timestamp() as usize,
        exp: expires_at.timestamp() as usize,
    };
    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(&config.jwt_secret),
    )
    .map_err(|error| AppError::Internal(format!("media token failed: {error}")))?;
    Ok((token, expires_at))
}

pub fn decode_media_token(token: &str, config: &Config) -> Result<MediaClaims> {
    let claims = decode::<MediaClaims>(
        token,
        &DecodingKey::from_secret(&config.jwt_secret),
        &Validation::new(Algorithm::HS256),
    )
    .map_err(|_| AppError::Unauthorized)?
    .claims;
    if claims.kind != "media" {
        return Err(AppError::Unauthorized);
    }
    Ok(claims)
}

pub fn session_cookie(token: &str, config: &Config) -> String {
    let mut value = format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
        config.session_ttl.as_secs()
    );
    if config.session_cookie_secure {
        value.push_str("; Secure");
    }
    value
}

pub fn expired_session_cookie(config: &Config) -> String {
    let mut value = format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0");
    if config.session_cookie_secure {
        value.push_str("; Secure");
    }
    value
}

pub async fn bootstrap_admin(state: &AppState) -> Result<()> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&state.pool)
        .await?;
    if count > 0 {
        return Ok(());
    }

    let password = state
        .config
        .bootstrap_admin_password
        .as_deref()
        .ok_or_else(|| {
            AppError::Internal(
                "BOOTSTRAP_ADMIN_PASSWORD is required while the users table is empty".into(),
            )
        })?;
    let hash = hash_password(password)?;
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, email, password_hash, role) VALUES (?, ?, ?, 'admin')")
        .bind(id)
        .bind(&state.config.bootstrap_admin_email)
        .bind(hash)
        .execute(&state.pool)
        .await?;
    tracing::info!(email = %state.config.bootstrap_admin_email, "bootstrap administrator created");
    Ok(())
}

fn decode_session(token: &str, config: &Config) -> Result<Uuid> {
    let claims = decode::<SessionClaims>(
        token,
        &DecodingKey::from_secret(&config.jwt_secret),
        &Validation::new(Algorithm::HS256),
    )
    .map_err(|_| AppError::Unauthorized)?
    .claims;
    if claims.kind != "session" {
        return Err(AppError::Unauthorized);
    }
    Uuid::parse_str(&claims.sub).map_err(|_| AppError::Unauthorized)
}

fn bearer_token(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::to_owned)
}

fn cookie_token(headers: &axum::http::HeaderMap) -> Option<String> {
    let cookies = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    cookies.split(';').find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        (name == SESSION_COOKIE).then(|| value.to_string())
    })
}
