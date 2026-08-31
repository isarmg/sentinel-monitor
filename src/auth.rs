use crate::{
    config::Config,
    error::{AppError, Result},
    protocol::CONTRACT,
    AppState,
};
use axum::{
    body::Body,
    extract::{FromRequestParts, State},
    http::{
        header::COOKIE, request::Parts, HeaderMap, HeaderName, HeaderValue, Method, Request, Uri,
    },
    middleware::Next,
    response::Response,
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use hkdf::Hkdf;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use sarmg_admin_auth::{
    is_token_shape, parse_cookie_value, random_token, require_administrator_same_origin,
    require_canonical_administrator_username, require_csrf_token_matches_hash,
    require_current_password_hash, token_hash, validate_password, AdministratorOriginMode,
    CSRF_HEADER, HOST_HEADER, ORIGIN_HEADER, SEC_FETCH_SITE_HEADER,
};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::time::Duration;
use uuid::Uuid;

const PRODUCTION_SESSION_COOKIE: &str = "__Host-sentinel_session";
const DEVELOPMENT_SESSION_COOKIE: &str = "sentinel_session";
const MEDIA_JWT_KEY_SALT: &[u8] = b"sentinel-monitor/0.2.0/media-jwt/signing-key";
const MEDIA_JWT_KEY_INFO: &[u8] = b"sentinel-media-jwt-v2/HS256";

#[derive(Clone)]
pub struct CurrentUser {
    pub id: Uuid,
    pub username: String,
    pub(crate) session_id: Uuid,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaClaims {
    pub protocol: String,
    pub iss: String,
    pub aud: String,
    pub kind: String,
    pub sub: Uuid,
    pub camera_id: Uuid,
    pub path: String,
    pub actions: Vec<String>,
    pub jti: Uuid,
    pub iat: u64,
    pub nbf: u64,
    pub exp: u64,
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
    username: String,
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

    validate_same_origin(request.headers(), request.uri(), &state.config)?;
    if request.uri().path() != admin_api_relative_path(sarmg_contracts::ADMIN_LOGIN_PATH) {
        let session = authenticate_browser_session(request.headers(), &state).await?;
        validate_csrf(request.headers(), &session.csrf_digest)?;
        request.extensions_mut().insert(session.user);
    }
    Ok(next.run(request).await)
}

fn admin_api_relative_path(path: &'static str) -> &'static str {
    path.strip_prefix("/api/v2")
        .expect("Foundation administrator path must use /api/v2")
}

pub fn hash_administrator_password(password: &str) -> Result<String> {
    validate_password(password).map_err(|error| AppError::Validation(error.to_string()))?;
    sarmg_admin_auth::hash_password(password)
        .map_err(|error| AppError::Internal(format!("password hash failed: {error}")))
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
    let token = random_token()
        .map_err(|error| AppError::Internal(format!("session token failed: {error}")))?;
    let csrf_token = random_token()
        .map_err(|error| AppError::Internal(format!("CSRF token failed: {error}")))?;
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
    .bind(token_hash(&token).to_vec())
    .bind(token_hash(&csrf_token).to_vec())
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

/// 为安全的会话查询签发新的 CSRF token。只保存摘要，明文只返回一次。
pub async fn rotate_csrf_token(state: &AppState, session_id: Uuid) -> Result<String> {
    let token = random_token()
        .map_err(|error| AppError::Internal(format!("CSRF token failed: {error}")))?;
    let changed = sqlx::query(
        "UPDATE browser_sessions SET csrf_digest = ? WHERE id = ? AND revoked_at IS NULL",
    )
    .bind(token_hash(&token).to_vec())
    .bind(session_id)
    .execute(&state.pool)
    .await?;
    if changed.rows_affected() != 1 {
        return Err(AppError::Unauthorized);
    }
    Ok(token)
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
        protocol: CONTRACT.media_jwt_protocol.clone(),
        iss: CONTRACT.media_jwt_issuer.clone(),
        aud: CONTRACT.media_jwt_audience.clone(),
        kind: CONTRACT.media_jwt_kind.clone(),
        sub: user_id,
        camera_id,
        path,
        actions,
        jti: Uuid::new_v4(),
        iat: now.timestamp() as u64,
        nbf: now.timestamp() as u64,
        exp: expires_at.timestamp() as u64,
    };
    let signing_key = media_signing_key(config);
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
    let signing_key = media_signing_key(config);
    let claims = decode::<MediaClaims>(token, &DecodingKey::from_secret(&signing_key), &validation)
        .map_err(|_| AppError::Unauthorized)?
        .claims;
    let now = u64::try_from(Utc::now().timestamp()).map_err(|_| AppError::Unauthorized)?;
    if claims.protocol != CONTRACT.media_jwt_protocol
        || claims.iss != CONTRACT.media_jwt_issuer
        || claims.aud != CONTRACT.media_jwt_audience
        || claims.kind != CONTRACT.media_jwt_kind
        || claims.sub.is_nil()
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

fn media_signing_key(config: &Config) -> [u8; 32] {
    let mut key = [0_u8; 32];
    Hkdf::<Sha256>::new(Some(MEDIA_JWT_KEY_SALT), &config.jwt_secret)
        .expand(MEDIA_JWT_KEY_INFO, &mut key)
        .expect("32-byte HKDF output is valid");
    key
}

#[cfg(test)]
pub(crate) fn encode_media_claims_for_test<T: Serialize>(claims: &T, config: &Config) -> String {
    let signing_key = media_signing_key(config);
    encode(
        &Header::new(Algorithm::HS256),
        claims,
        &EncodingKey::from_secret(&signing_key),
    )
    .expect("encode test media claims")
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

pub fn expired_session_cookie(config: &Config) -> String {
    persistent_cookie(session_cookie_name(config), "", config, true, 0)
}

pub fn session_cookie_name(config: &Config) -> &'static str {
    if config.development_mode {
        DEVELOPMENT_SESSION_COOKIE
    } else {
        PRODUCTION_SESSION_COOKIE
    }
}

pub async fn bootstrap_admin(state: &AppState) -> Result<()> {
    validate_persisted_administrator_credentials(state).await?;
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
    let hash = hash_administrator_password(password)?;
    let id = Uuid::new_v4();
    let now = Utc::now();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, active, created_at, updated_at) \
         VALUES (?, ?, ?, 1, ?, ?)",
    )
    .bind(id)
    .bind(&state.config.bootstrap_admin_username)
    .bind(hash)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await?;
    tracing::info!(username = %state.config.bootstrap_admin_username, "bootstrap administrator created");
    Ok(())
}

/// Validate every persisted control-plane identity before the listener starts.
/// There is deliberately no alternate username or password-hash fallback.
async fn validate_persisted_administrator_credentials(state: &AppState) -> Result<()> {
    let credentials = sqlx::query_as::<_, (Uuid, String, String)>(
        "SELECT id, username, password_hash FROM users ORDER BY id",
    )
    .fetch_all(&state.pool)
    .await?;

    for (id, username, password_hash) in credentials {
        require_canonical_administrator_username(&username).map_err(|error| {
            AppError::Internal(format!(
                "persisted administrator {id} has a non-canonical username: {error}"
            ))
        })?;
        require_current_password_hash(&password_hash).map_err(|error| {
            AppError::Internal(format!(
                "persisted administrator {id} has a non-current password hash: {error}"
            ))
        })?;
    }
    Ok(())
}

async fn authenticate_browser_session(
    headers: &HeaderMap,
    state: &AppState,
) -> Result<AuthenticatedBrowserSession> {
    let token =
        cookie_token(headers, session_cookie_name(&state.config)).ok_or(AppError::Unauthorized)?;
    let now = Utc::now();
    let row = sqlx::query_as::<_, SessionUserRow>(
        "SELECT s.id AS session_id, s.csrf_digest, s.absolute_expires_at, \
                u.id AS user_id, u.username \
         FROM browser_sessions s \
         JOIN users u ON u.id = s.user_id \
         WHERE s.token_digest = ? AND s.revoked_at IS NULL \
           AND s.session_version = u.session_version AND u.active = 1 \
           AND s.idle_expires_at > ? AND s.absolute_expires_at > ?",
    )
    .bind(token_hash(&token).to_vec())
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
            username: row.username,
            session_id: row.session_id,
        },
    })
}

fn validate_csrf(headers: &HeaderMap, expected_digest: &[u8]) -> Result<()> {
    let name = HeaderName::from_static(CSRF_HEADER);
    let values = raw_header_values(headers, &name);
    require_csrf_token_matches_hash(&values, expected_digest).map_err(|_| AppError::Forbidden)
}

fn validate_same_origin(headers: &HeaderMap, uri: &Uri, config: &Config) -> Result<()> {
    let mode = if config.development_mode {
        AdministratorOriginMode::LoopbackDevelopmentHttp
    } else {
        AdministratorOriginMode::ProductionHttps
    };
    let origin_name = HeaderName::from_static(ORIGIN_HEADER);
    let host_name = HeaderName::from_static(HOST_HEADER);
    let site_name = HeaderName::from_static(SEC_FETCH_SITE_HEADER);
    let origins = raw_header_values(headers, &origin_name);
    let mut hosts = raw_header_values(headers, &host_name);
    if let Some(authority) = uri.authority() {
        hosts.push(authority.as_str().as_bytes());
    }
    let sites = raw_header_values(headers, &site_name);
    require_administrator_same_origin(mode, &origins, &hosts, &sites)
        .map(|_| ())
        .map_err(|_| AppError::Forbidden)
}

fn is_state_changing(method: &Method) -> bool {
    matches!(
        method,
        &Method::POST | &Method::PUT | &Method::PATCH | &Method::DELETE
    )
}

fn cookie_token(headers: &HeaderMap, cookie_name: &str) -> Option<String> {
    let cookies = single_cookie_header(headers)?;
    parse_cookie_value(cookies, cookie_name)
        .filter(|value| is_token_shape(value))
        .map(str::to_owned)
}

fn single_cookie_header(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(COOKIE).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    Some(value)
}

fn raw_header_values<'headers>(
    headers: &'headers HeaderMap,
    name: &HeaderName,
) -> Vec<&'headers [u8]> {
    headers
        .get_all(name)
        .iter()
        .map(HeaderValue::as_bytes)
        .collect()
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

#[cfg(test)]
mod tests {
    use super::{cookie_token, random_token, MediaClaims};
    use axum::http::{header::COOKIE, HeaderMap, HeaderValue};
    use serde_json::json;
    use uuid::Uuid;

    fn current_claims() -> serde_json::Value {
        json!({
            "protocol": "sentinel-media-jwt-v2",
            "iss": "sentinel-monitor/0.2.0",
            "aud": "sentinel-mediamtx/1.20.0",
            "kind": "media",
            "sub": Uuid::new_v4(),
            "camera_id": Uuid::new_v4(),
            "path": "camera/main",
            "actions": ["read"],
            "jti": Uuid::new_v4(),
            "iat": 1,
            "nbf": 1,
            "exp": 121
        })
    }

    #[test]
    fn media_claims_reject_unknown_and_missing_protocol_fields() {
        let mut unknown = current_claims();
        unknown
            .as_object_mut()
            .expect("claims object")
            .insert("unknown_field".into(), json!(true));
        assert!(serde_json::from_value::<MediaClaims>(unknown).is_err());

        let mut missing = current_claims();
        missing
            .as_object_mut()
            .expect("claims object")
            .remove("jti");
        assert!(serde_json::from_value::<MediaClaims>(missing).is_err());
    }

    #[test]
    fn browser_cookie_requires_one_header_one_name_and_current_token_shape() {
        let token = random_token().expect("generate canonical session token");
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            HeaderValue::from_str(&format!("other=x; sentinel_session={token}"))
                .expect("valid test Cookie header"),
        );
        assert_eq!(
            cookie_token(&headers, "sentinel_session"),
            Some(token.clone())
        );

        headers.insert(
            COOKIE,
            HeaderValue::from_str(&format!(
                "sentinel_session={token}; sentinel_session={token}"
            ))
            .expect("valid duplicate-cookie test header"),
        );
        assert_eq!(cookie_token(&headers, "sentinel_session"), None);

        headers.insert(COOKIE, HeaderValue::from_static("sentinel_session=short"));
        assert_eq!(cookie_token(&headers, "sentinel_session"), None);

        headers.insert(
            COOKIE,
            HeaderValue::from_str(&format!("sentinel_session={token}"))
                .expect("valid first Cookie header"),
        );
        headers.append(
            COOKIE,
            HeaderValue::from_str(&format!("sentinel_session={token}"))
                .expect("valid second Cookie header"),
        );
        assert_eq!(cookie_token(&headers, "sentinel_session"), None);
    }
}
