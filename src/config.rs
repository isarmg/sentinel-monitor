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
    pub session_cookie_secure: bool,
    pub session_ttl: Duration,
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
            bind_addr: value("BIND_ADDR", "0.0.0.0:8080")
                .parse()
                .map_err(|_| "BIND_ADDR is invalid".to_string())?,
            database_url: required("DATABASE_URL")?,
            jwt_secret,
            credentials_key,
            bootstrap_admin_email: value("BOOTSTRAP_ADMIN_EMAIL", "admin@example.com")
                .trim()
                .to_lowercase(),
            bootstrap_admin_password: env::var("BOOTSTRAP_ADMIN_PASSWORD").ok(),
            session_cookie_secure: parse_bool("SESSION_COOKIE_SECURE", false)?,
            session_ttl: Duration::from_secs(parse_u64("SESSION_TTL_HOURS", 12)? * 3600),
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

fn parse_bool(name: &str, default: bool) -> Result<bool, String> {
    value(name, if default { "true" } else { "false" })
        .parse()
        .map_err(|_| format!("{name} must be true or false"))
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
