use base64::{engine::general_purpose::STANDARD, Engine as _};
use ipnet::IpNet;
use sarmg_admin_auth::normalize_administrator_username;
use std::{env, net::SocketAddr, path::PathBuf, time::Duration};

const DEFAULT_BIND_ADDR: &str = "127.0.0.1:8080";

#[derive(Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
    pub database_url: String,
    pub jwt_secret: Vec<u8>,
    pub credentials_key: [u8; 32],
    pub runtime_directory: PathBuf,
    pub bootstrap_admin_username: String,
    pub bootstrap_admin_password: Option<String>,
    pub development_mode: bool,
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
        // The public TLS/media gateway is the only network-facing entry. A
        // missing BIND_ADDR therefore stays on loopback in every environment.
        let bind_addr = value("BIND_ADDR", DEFAULT_BIND_ADDR)
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
        let runtime_directory = PathBuf::from(required("SENTINEL_RUNTIME_DIR")?);
        if !runtime_directory.is_absolute() {
            return Err("SENTINEL_RUNTIME_DIR must be an absolute path".into());
        }
        let bootstrap_admin_password = env::var("BOOTSTRAP_ADMIN_PASSWORD").ok();
        if let Some(password) = &bootstrap_admin_password {
            sarmg_admin_auth::validate_password(password)
                .map_err(|error| format!("BOOTSTRAP_ADMIN_PASSWORD is invalid: {error}"))?;
        }

        Ok(Self {
            bind_addr,
            database_url: required("DATABASE_URL")?,
            jwt_secret,
            credentials_key,
            runtime_directory,
            bootstrap_admin_username: normalize_administrator_username(&value(
                "BOOTSTRAP_ADMIN_USERNAME",
                "admin",
            ))
            .map_err(|error| format!("BOOTSTRAP_ADMIN_USERNAME is invalid: {error}"))?,
            bootstrap_admin_password,
            development_mode,
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
            request_timeout: Duration::from_secs(bounded_u64("REQUEST_TIMEOUT_SECS", 20, 1, 300)?),
            onvif_discovery_timeout: Duration::from_millis(parse_u64(
                "ONVIF_DISCOVERY_TIMEOUT_MS",
                3000,
            )?),
            onvif_xaddr_allowlist: parse_cidr_list("ONVIF_XADDR_ALLOWLIST")?,
            static_dir: absolute_path("STATIC_DIR", required("STATIC_DIR")?)?,
        })
    }
}

fn absolute_path(name: &str, value: String) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(format!("{name} must be an absolute path"))
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

fn bounded_u64(name: &str, default: u64, minimum: u64, maximum: u64) -> Result<u64, String> {
    let value = parse_u64(name, default)?;
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

    #[test]
    fn default_server_binding_is_loopback() {
        let address = DEFAULT_BIND_ADDR.parse::<SocketAddr>().unwrap();
        assert!(address.ip().is_loopback());
        assert_eq!(address.port(), 8080);
    }

    #[test]
    fn runtime_paths_are_absolute() {
        assert_eq!(
            absolute_path("STATIC_DIR", "/opt/sentinel/web".into()).unwrap(),
            PathBuf::from("/opt/sentinel/web")
        );
        assert_eq!(
            absolute_path("STATIC_DIR", "web/dist".into()).unwrap_err(),
            "STATIC_DIR must be an absolute path"
        );
    }
}
