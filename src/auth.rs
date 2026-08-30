use crate::{
    config::Config,
    error::{AppError, Result},
    models::UserView,
    AppState,
};
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use axum::{
    body::Body,
    extract::{FromRequestParts, State},
    http::{
        header::{COOKIE, HOST, ORIGIN},
        request::Parts,
        uri::Authority,
        HeaderMap, Method, Request,
    },
    middleware::Next,
    response::Response,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::Duration;
use subtle::ConstantTimeEq;
use url::Url;
use uuid::Uuid;

const PRODUCTION_SESSION_COOKIE: &str = "__Host-sentinel_session";
const DEVELOPMENT_SESSION_COOKIE: &str = "sentinel_session";
const PRODUCTION_CSRF_COOKIE: &str = "__Host-sentinel_csrf";
const DEVELOPMENT_CSRF_COOKIE: &str = "sentinel_csrf";
const TOKEN_BYTES: usize = 32;
const MAX_TOKEN_LENGTH: usize = 128;

#[derive(Clone)]
pub struct CurrentUser {
    pub id: Uuid,
    pub email: String,
    pub role: String,
    pub active: bool,
    pub last_login_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub(crate) session_id: Uuid,
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
            active: self.active,
            last_login_at: self.last_login_at,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
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

pub struct IssuedBrowserSession {
    #[cfg(test)]
    pub session_id: Uuid,
    pub token: String,
    pub csrf_token: String,
}

#[derive(sqlx::FromRow)]
struct SessionUserRow {
    session_id: Uuid,
    csrf_digest: Vec<u8>,
    absolute_expires_at: DateTime<Utc>,
    user_id: Uuid,
    email: String,
    role: String,
    active: bool,
    last_login_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

struct AuthenticatedBrowserSession {
    user: CurrentUser,
    csrf_digest: Vec<u8>,
}

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self> {
        if let Some(user) = parts.extensions.get::<Self>() {
            return Ok(user.clone());
        }
        Ok(authenticate_browser_session(&parts.headers, state)
            .await?
            .user)
    }
}

pub async fn enforce_browser_security(
    State(state): State<AppState>,
    mut request: Request<Body>,
    next: Next,
) -> Result<Response> {
    if !is_state_changing(request.method()) {
        return Ok(next.run(request).await);
    }

    validate_same_origin(request.headers())?;
    if !matches!(request.uri().path(), "/auth/login" | "/api/auth/login") {
        let session = authenticate_browser_session(request.headers(), &state).await?;
        validate_csrf(request.headers(), &session.csrf_digest)?;
        request.extensions_mut().insert(session.user);
    }
    Ok(next.run(request).await)
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

pub async fn issue_session(
    state: &AppState,
    user_id: Uuid,
    session_version: i64,
) -> Result<IssuedBrowserSession> {
    let now = Utc::now();
    let absolute_expires_at = now + chrono_duration(state.config.session_absolute_ttl)?;
    let idle_expires_at = std::cmp::min(
        now + chrono_duration(state.config.session_idle_ttl)?,
        absolute_expires_at,
    );
    let token = random_token();
    let csrf_token = random_token();
    let session_id = Uuid::new_v4();

    let mut transaction = state.pool.begin().await?;
    sqlx::query(
        "DELETE FROM browser_sessions \
         WHERE user_id = ? AND (revoked_at IS NOT NULL OR idle_expires_at <= ? OR absolute_expires_at <= ?)",
    )
    .bind(user_id)
    .bind(now)
    .bind(now)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO browser_sessions \
         (id, user_id, token_digest, csrf_digest, session_version, created_at, last_seen_at, idle_expires_at, absolute_expires_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(session_id)
    .bind(user_id)
    .bind(token_digest(&token).to_vec())
    .bind(token_digest(&csrf_token).to_vec())
    .bind(session_version)
    .bind(now)
    .bind(now)
    .bind(idle_expires_at)
    .bind(absolute_expires_at)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    Ok(IssuedBrowserSession {
        #[cfg(test)]
        session_id,
        token,
        csrf_token,
    })
}

pub async fn revoke_session(state: &AppState, session_id: Uuid) -> Result<()> {
    sqlx::query("UPDATE browser_sessions SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL")
        .bind(Utc::now())
        .bind(session_id)
        .execute(&state.pool)
        .await?;
    Ok(())
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
    persistent_cookie(
        session_cookie_name(config),
        token,
        config,
        true,
        state_cookie_max_age(config),
    )
}

pub fn csrf_cookie(token: &str, config: &Config) -> String {
    persistent_cookie(
        csrf_cookie_name(config),
        token,
        config,
        false,
        state_cookie_max_age(config),
    )
}

pub fn expired_session_cookie(config: &Config) -> String {
    persistent_cookie(session_cookie_name(config), "", config, true, 0)
}

pub fn expired_csrf_cookie(config: &Config) -> String {
    persistent_cookie(csrf_cookie_name(config), "", config, false, 0)
}

pub fn session_cookie_name(config: &Config) -> &'static str {
    if config.development_mode {
        DEVELOPMENT_SESSION_COOKIE
    } else {
        PRODUCTION_SESSION_COOKIE
    }
}

pub fn csrf_cookie_name(config: &Config) -> &'static str {
    if config.development_mode {
        DEVELOPMENT_CSRF_COOKIE
    } else {
        PRODUCTION_CSRF_COOKIE
    }
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
    let now = Utc::now();
    sqlx::query(
        "INSERT INTO users (id, email, password_hash, role, active, created_at, updated_at) \
         VALUES (?, ?, ?, 'admin', 1, ?, ?)",
    )
    .bind(id)
    .bind(&state.config.bootstrap_admin_email)
    .bind(hash)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await?;
    tracing::info!(email = %state.config.bootstrap_admin_email, "bootstrap administrator created");
    Ok(())
}

async fn authenticate_browser_session(
    headers: &HeaderMap,
    state: &AppState,
) -> Result<AuthenticatedBrowserSession> {
    let token = cookie_token(headers, session_cookie_name(&state.config))
        .filter(|token| !token.is_empty() && token.len() <= MAX_TOKEN_LENGTH)
        .ok_or(AppError::Unauthorized)?;
    let now = Utc::now();
    let row = sqlx::query_as::<_, SessionUserRow>(
        "SELECT s.id AS session_id, s.csrf_digest, s.absolute_expires_at, \
                u.id AS user_id, u.email, u.role, u.active, u.last_login_at, u.created_at, u.updated_at \
         FROM browser_sessions s \
         JOIN users u ON u.id = s.user_id \
         WHERE s.token_digest = ? AND s.revoked_at IS NULL \
           AND s.session_version = u.session_version AND u.active = 1 \
           AND s.idle_expires_at > ? AND s.absolute_expires_at > ?",
    )
    .bind(token_digest(&token).to_vec())
    .bind(now)
    .bind(now)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(AppError::Unauthorized)?;

    let next_idle_expiry = std::cmp::min(
        now + chrono_duration(state.config.session_idle_ttl)?,
        row.absolute_expires_at,
    );
    let updated = sqlx::query(
        "UPDATE browser_sessions \
         SET last_seen_at = MAX(last_seen_at, ?), \
             idle_expires_at = MIN(absolute_expires_at, MAX(idle_expires_at, ?)) \
         WHERE id = ? AND revoked_at IS NULL AND idle_expires_at > ? AND absolute_expires_at > ? \
           AND session_version = (SELECT session_version FROM users WHERE id = user_id AND active = 1)",
    )
    .bind(now)
    .bind(next_idle_expiry)
    .bind(row.session_id)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(AppError::Unauthorized);
    }

    Ok(AuthenticatedBrowserSession {
        csrf_digest: row.csrf_digest,
        user: CurrentUser {
            id: row.user_id,
            email: row.email,
            role: row.role,
            active: row.active,
            last_login_at: row.last_login_at,
            created_at: row.created_at,
            updated_at: row.updated_at,
            session_id: row.session_id,
        },
    })
}

fn validate_csrf(headers: &HeaderMap, expected_digest: &[u8]) -> Result<()> {
    let token = headers
        .get("x-csrf-token")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty() && value.len() <= MAX_TOKEN_LENGTH)
        .ok_or(AppError::Forbidden)?;
    let digest = token_digest(token);
    if expected_digest.len() != digest.len()
        || expected_digest.ct_eq(digest.as_slice()).unwrap_u8() != 1
    {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

fn validate_same_origin(headers: &HeaderMap) -> Result<()> {
    let host = headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .ok_or(AppError::Forbidden)?;
    let authority = host.parse::<Authority>().map_err(|_| AppError::Forbidden)?;
    let raw_origin = headers
        .get(ORIGIN)
        .and_then(|value| value.to_str().ok())
        .ok_or(AppError::Forbidden)?;
    let origin = Url::parse(raw_origin).map_err(|_| AppError::Forbidden)?;
    if !matches!(origin.scheme(), "http" | "https")
        || !origin.username().is_empty()
        || origin.password().is_some()
        || origin.path() != "/"
        || origin.query().is_some()
        || origin.fragment().is_some()
    {
        return Err(AppError::Forbidden);
    }
    let origin_host = origin.host_str().ok_or(AppError::Forbidden)?;
    if normalize_host(origin_host) != normalize_host(authority.host()) {
        return Err(AppError::Forbidden);
    }
    match authority.port_u16() {
        Some(port) if origin.port_or_known_default() != Some(port) => Err(AppError::Forbidden),
        None if origin.port().is_some() => Err(AppError::Forbidden),
        _ => Ok(()),
    }
}

fn normalize_host(host: &str) -> String {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase()
}

fn is_state_changing(method: &Method) -> bool {
    matches!(
        method,
        &Method::POST | &Method::PUT | &Method::PATCH | &Method::DELETE
    )
}

fn random_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn token_digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn cookie_token(headers: &HeaderMap, cookie_name: &str) -> Option<String> {
    let cookies = headers.get(COOKIE)?.to_str().ok()?;
    cookies.split(';').find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        (name == cookie_name).then(|| value.to_string())
    })
}

fn persistent_cookie(
    name: &str,
    value: &str,
    config: &Config,
    http_only: bool,
    max_age: u64,
) -> String {
    let mut cookie = format!("{name}={value}; Path=/; SameSite=Strict; Max-Age={max_age}");
    if http_only {
        cookie.push_str("; HttpOnly");
    }
    if !config.development_mode {
        cookie.push_str("; Secure");
    }
    cookie
}

fn state_cookie_max_age(config: &Config) -> u64 {
    config.session_absolute_ttl.as_secs()
}

fn chrono_duration(duration: Duration) -> Result<ChronoDuration> {
    ChronoDuration::from_std(duration)
        .map_err(|_| AppError::Internal("invalid session duration".into()))
}
