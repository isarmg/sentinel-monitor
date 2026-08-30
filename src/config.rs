use base64::{engine::general_purpose::STANDARD, Engine as _};
use ipnet::IpNet;
use std::{env, net::SocketAddr, path::PathBuf, time::Duration};

#[derive(Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
    pub database_url: String,
    pub jwt_secret: Vec<u8>,
    pub credentials_key: [u8; 32],
    pub bootstrap_admin_email: String,
    pub bootstrap_admin_password: Option<String>,
    pub development_mode: bool,
    pub session_idle_ttl: Duration,
    pub session_absolute_ttl: Duration,
    pub login_body_limit: usize,
    pub login_rate_capacity: usize,
    pub login_source_attempts: u32,
    pub login_source_window: Duration,
    pub login_account_attempts: u32,
    pub login_account_window: Duration,
    pub login_argon2_parallelism: usize,
    pub login_argon2_timeout: Duration,
    pub media_token_ttl: Duration,
    pub mediamtx_api_url: String,
    pub mediamtx_playback_url: String,
    pub public_webrtc_base_url: String,
    pub public_hls_base_url: String,
    pub status_interval: Duration,
    pub reconcile_interval: Duration,
    pub request_timeout: Duration,
    pub onvif_discovery_timeout: Duration,
    pub onvif_xaddr_allowlist: Vec<IpNet>,
    pub static_dir: PathBuf,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let bind_addr = value("BIND_ADDR", "0.0.0.0:8080")
            .parse::<SocketAddr>()
            .map_err(|_| "BIND_ADDR is invalid".to_string())?;
        let development_mode = match value("APP_ENV", "production").as_str() {
            "production" => false,
            "development" => true,
            _ => return Err("APP_ENV must be production or development".into()),
        };
        validate_development_bind(bind_addr, development_mode)?;

        let jwt_secret = required("APP_JWT_SECRET")?.into_bytes();
        if jwt_secret.len() < 32 {
            return Err("APP_JWT_SECRET must contain at least 32 bytes".into());
        }

        let decoded_key = STANDARD
            .decode(required("CREDENTIALS_KEY")?)
            .map_err(|_| "CREDENTIALS_KEY must be valid base64".to_string())?;
        let credentials_key: [u8; 32] = decoded_key
            .try_into()
            .map_err(|_| "CREDENTIALS_KEY must decode to exactly 32 bytes".to_string())?;

        Ok(Self {
            bind_addr,
            database_url: required("DATABASE_URL")?,
            jwt_secret,
            credentials_key,
            bootstrap_admin_email: value("BOOTSTRAP_ADMIN_EMAIL", "admin@example.com")
                .trim()
                .to_lowercase(),
            bootstrap_admin_password: env::var("BOOTSTRAP_ADMIN_PASSWORD").ok(),
            development_mode,
            session_idle_ttl: duration_from_env("SESSION_IDLE_TTL_MINUTES", 30, 60)?,
            session_absolute_ttl: duration_from_env("SESSION_ABSOLUTE_TTL_HOURS", 12, 3_600)?,
            login_body_limit: bounded_usize("LOGIN_BODY_LIMIT_BYTES", 16_384, 1_024, 65_536)?,
            login_rate_capacity: bounded_usize("LOGIN_RATE_CAPACITY", 4_096, 128, 65_536)?,
            login_source_attempts: bounded_u64("LOGIN_SOURCE_ATTEMPTS", 30, 1, 1_000)? as u32,
            login_source_window: Duration::from_secs(bounded_u64(
                "LOGIN_SOURCE_WINDOW_SECS",
                60,
                1,
                3_600,
            )?),
            login_account_attempts: bounded_u64("LOGIN_ACCOUNT_ATTEMPTS", 10, 1, 1_000)? as u32,
            login_account_window: Duration::from_secs(bounded_u64(
                "LOGIN_ACCOUNT_WINDOW_SECS",
                300,
                1,
                86_400,
            )?),
            login_argon2_parallelism: bounded_usize("LOGIN_ARGON2_PARALLELISM", 2, 1, 64)?,
            login_argon2_timeout: Duration::from_millis(bounded_u64(
                "LOGIN_ARGON2_TIMEOUT_MS",
                5_000,
                100,
                30_000,
            )?),
            media_token_ttl: Duration::from_secs(parse_u64("MEDIA_TOKEN_TTL_SECS", 120)?),
            mediamtx_api_url: trim_slash(value("MEDIAMTX_API_URL", "http://127.0.0.1:9997")),
            mediamtx_playback_url: trim_slash(value(
                "MEDIAMTX_PLAYBACK_URL",
                "http://127.0.0.1:9996",
            )),
            public_webrtc_base_url: trim_slash(value("PUBLIC_WEBRTC_BASE_URL", "/media-webrtc")),
            public_hls_base_url: trim_slash(value("PUBLIC_HLS_BASE_URL", "/media-hls")),
            status_interval: Duration::from_secs(parse_u64("STATUS_INTERVAL_SECS", 10)?),
            reconcile_interval: Duration::from_secs(parse_u64("RECONCILE_INTERVAL_SECS", 60)?),
            request_timeout: Duration::from_secs(parse_u64("REQUEST_TIMEOUT_SECS", 20)?),
            onvif_discovery_timeout: Duration::from_millis(parse_u64(
                "ONVIF_DISCOVERY_TIMEOUT_MS",
                3000,
            )?),
            onvif_xaddr_allowlist: parse_cidr_list("ONVIF_XADDR_ALLOWLIST")?,
            static_dir: PathBuf::from(value("STATIC_DIR", "web/dist")),
        })
    }
}

fn required(name: &str) -> Result<String, String> {
    env::var(name).map_err(|_| format!("{name} is required"))
}

fn value(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_string())
}

fn parse_u64(name: &str, default: u64) -> Result<u64, String> {
    value(name, &default.to_string())
        .parse()
        .map_err(|_| format!("{name} must be an unsigned integer"))
}

fn duration_from_env(name: &str, default: u64, unit_seconds: u64) -> Result<Duration, String> {
    let value = parse_u64(name, default)?;
    let seconds = value
        .checked_mul(unit_seconds)
        .ok_or_else(|| format!("{name} is too large"))?;
    if seconds == 0 {
        return Err(format!("{name} must be greater than zero"));
    }
    Ok(Duration::from_secs(seconds))
}

fn bounded_u64(name: &str, default: u64, minimum: u64, maximum: u64) -> Result<u64, String> {
    let value = parse_u64(name, default)?;
    if (minimum..=maximum).contains(&value) {
        Ok(value)
    } else {
        Err(format!("{name} must be between {minimum} and {maximum}"))
    }
}

fn bounded_usize(
    name: &str,
    default: u64,
    minimum: usize,
    maximum: usize,
) -> Result<usize, String> {
    let value = parse_u64(name, default)?;
    let value = usize::try_from(value).map_err(|_| format!("{name} is too large"))?;
    if (minimum..=maximum).contains(&value) {
        Ok(value)
    } else {
        Err(format!("{name} must be between {minimum} and {maximum}"))
    }
}

fn validate_development_bind(bind_addr: SocketAddr, development_mode: bool) -> Result<(), String> {
    if development_mode && !bind_addr.ip().is_loopback() {
        Err("development mode must bind to a loopback address".into())
    } else {
        Ok(())
    }
}

fn parse_cidr_list(name: &str) -> Result<Vec<IpNet>, String> {
    env::var(name)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse()
                .map_err(|_| format!("{name} must be a comma-separated CIDR list"))
        })
        .collect()
}

fn trim_slash(mut value: String) -> String {
    while value.ends_with('/') {
        value.pop();
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn development_mode_only_accepts_loopback_bindings() {
        assert!(validate_development_bind("127.0.0.1:8080".parse().unwrap(), true).is_ok());
        assert!(validate_development_bind("[::1]:8080".parse().unwrap(), true).is_ok());
        assert!(validate_development_bind("0.0.0.0:8080".parse().unwrap(), true).is_err());
        assert!(validate_development_bind("192.168.1.10:8080".parse().unwrap(), true).is_err());
        assert!(validate_development_bind("0.0.0.0:8080".parse().unwrap(), false).is_ok());
    }
}
