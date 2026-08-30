use serde::Deserialize;
use std::sync::LazyLock;

const CONTRACT_SOURCE: &str = include_str!("../web/src/protocol-contract.json");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolContract {
    pub wire_protocol: String,
    pub api_prefix: String,
    pub media_auth_path: String,
    pub media_jwt_protocol: String,
    pub media_jwt_issuer: String,
    pub media_jwt_audience: String,
    pub media_jwt_kind: String,
}

pub static CONTRACT: LazyLock<ProtocolContract> = LazyLock::new(|| {
    let contract: ProtocolContract = serde_json::from_str(CONTRACT_SOURCE)
        .expect("embedded protocol contract must be valid JSON");
    assert_eq!(contract.wire_protocol, "sentinel-wire-v2");
    assert_eq!(contract.api_prefix, "/api/v2");
    assert_eq!(contract.media_auth_path, "/internal/v2/media/auth");
    assert_eq!(contract.media_jwt_protocol, "sentinel-media-jwt-v2");
    assert_eq!(contract.media_jwt_issuer, "sentinel-monitor/0.2.0");
    assert_eq!(contract.media_jwt_audience, "sentinel-mediamtx/1.20.0");
    assert_eq!(contract.media_jwt_kind, "media");
    contract
});

#[cfg(test)]
mod tests {
    use super::CONTRACT;

    #[test]
    fn rust_web_and_mediamtx_share_the_current_protocol_contract() {
        assert_eq!(
            CONTRACT.media_jwt_issuer,
            format!("sentinel-monitor/{}", env!("CARGO_PKG_VERSION"))
        );

        let web = include_str!("../web/src/main.js");
        assert!(web.contains("import { apiPath } from \"./protocol.js\";"));
        assert!(!web.contains("\"/api/"));
        let vite = include_str!("../web/vite.config.js");
        assert!(vite.contains("\"/api/v2\": \"http://127.0.0.1:8080\""));
        assert!(!vite.contains("\"/api\":"));

        let routes = include_str!("routes.rs");
        assert!(routes.contains(".nest(&CONTRACT.api_prefix, api)"));
        assert!(routes.contains(".route(&CONTRACT.media_auth_path, post(media_auth))"));

        for config in [
            include_str!("../native/mediamtx.yml"),
            include_str!("../mediamtx.yml"),
        ] {
            assert!(
                config.lines().any(|line| line
                    .strip_prefix("authHTTPAddress: ")
                    .is_some_and(|url| url.ends_with(&CONTRACT.media_auth_path))),
                "MediaMTX config must use the embedded current callback path"
            );
        }
    }
}
