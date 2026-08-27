use crate::error::{AppError, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::{SecondsFormat, Utc};
use rand::{rngs::OsRng, RngCore};
use reqwest::Client;
use roxmltree::Document;
use serde::Serialize;
use sha1::{Digest, Sha1};
use std::{collections::HashSet, net::SocketAddr, time::Duration};
use tokio::{net::UdpSocket, time};
use uuid::Uuid;

#[derive(Clone, Serialize)]
pub struct DiscoveredDevice {
    pub endpoint: String,
    pub xaddrs: Vec<String>,
    pub scopes: Vec<String>,
    pub remote_addr: String,
}

pub async fn discover(timeout: Duration) -> Result<Vec<DiscoveredDevice>> {
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|error| AppError::Internal(format!("ONVIF discovery bind failed: {error}")))?;
    socket
        .set_broadcast(true)
        .map_err(|error| AppError::Internal(format!("ONVIF socket setup failed: {error}")))?;

    let message_id = Uuid::new_v4();
    let probe = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope" xmlns:w="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery" xmlns:dn="http://www.onvif.org/ver10/network/wsdl">
  <e:Header><w:MessageID>uuid:{message_id}</w:MessageID><w:To e:mustUnderstand="true">urn:schemas-xmlsoap-org:ws:2005:04:discovery</w:To><w:Action e:mustUnderstand="true">http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</w:Action></e:Header>
  <e:Body><d:Probe><d:Types>dn:NetworkVideoTransmitter</d:Types></d:Probe></e:Body>
</e:Envelope>"#
    );
    socket
        .send_to(probe.as_bytes(), "239.255.255.250:3702")
        .await
        .map_err(|error| AppError::Internal(format!("ONVIF probe failed: {error}")))?;

    let deadline = time::Instant::now() + timeout;
    let mut buffer = vec![0u8; 64 * 1024];
    let mut seen = HashSet::new();
    let mut devices = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let received = time::timeout(remaining, socket.recv_from(&mut buffer)).await;
        let Ok(Ok((length, remote))) = received else {
            break;
        };
        if let Some(device) = parse_probe_response(&buffer[..length], remote) {
            let key = device
                .xaddrs
                .first()
                .cloned()
                .unwrap_or_else(|| device.endpoint.clone());
            if seen.insert(key) {
                devices.push(device);
            }
        }
    }
    Ok(devices)
}

pub async fn ptz(
    client: &Client,
    device_service_url: &str,
    username: Option<&str>,
    password: Option<&str>,
    action: &str,
    pan: f64,
    tilt: f64,
    zoom: f64,
) -> Result<()> {
    let capabilities_body = r#"<tds:GetCapabilities xmlns:tds="http://www.onvif.org/ver10/device/wsdl"><tds:Category>All</tds:Category></tds:GetCapabilities>"#;
    let capabilities = soap_request(
        client,
        device_service_url,
        "http://www.onvif.org/ver10/device/wsdl/GetCapabilities",
        capabilities_body,
        username,
        password,
    )
    .await?;
    let (media_url, ptz_url) = parse_capability_urls(&capabilities)?;

    let profiles = soap_request(
        client,
        &media_url,
        "http://www.onvif.org/ver10/media/wsdl/GetProfiles",
        r#"<trt:GetProfiles xmlns:trt="http://www.onvif.org/ver10/media/wsdl"/>"#,
        username,
        password,
    )
    .await?;
    let profile_token = parse_profile_token(&profiles)?;
    let token = xml_escape(&profile_token);

    let body = if action == "stop" {
        format!(
            r#"<tptz:Stop xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl"><tptz:ProfileToken>{token}</tptz:ProfileToken><tptz:PanTilt>true</tptz:PanTilt><tptz:Zoom>true</tptz:Zoom></tptz:Stop>"#
        )
    } else {
        format!(
            r#"<tptz:ContinuousMove xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema"><tptz:ProfileToken>{token}</tptz:ProfileToken><tptz:Velocity><tt:PanTilt x="{pan}" y="{tilt}"/><tt:Zoom x="{zoom}"/></tptz:Velocity><tptz:Timeout>PT1S</tptz:Timeout></tptz:ContinuousMove>"#
        )
    };
    let soap_action = if action == "stop" {
        "http://www.onvif.org/ver20/ptz/wsdl/Stop"
    } else {
        "http://www.onvif.org/ver20/ptz/wsdl/ContinuousMove"
    };
    soap_request(client, &ptz_url, soap_action, &body, username, password).await?;
    Ok(())
}

fn parse_probe_response(bytes: &[u8], remote: SocketAddr) -> Option<DiscoveredDevice> {
    let xml = std::str::from_utf8(bytes).ok()?;
    let document = Document::parse(xml).ok()?;
    let text = |name: &str| {
        document
            .descendants()
            .find(|node| node.is_element() && node.tag_name().name() == name)
            .and_then(|node| node.text())
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let xaddrs = text("XAddrs")
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>();
    if xaddrs.is_empty() {
        return None;
    }
    Some(DiscoveredDevice {
        endpoint: text("Address"),
        xaddrs,
        scopes: text("Scopes")
            .split_whitespace()
            .map(str::to_string)
            .collect(),
        remote_addr: remote.to_string(),
    })
}

async fn soap_request(
    client: &Client,
    url: &str,
    action: &str,
    body: &str,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<String> {
    let security = match (username, password) {
        (Some(username), Some(password)) if !username.is_empty() => {
            ws_security_header(username, password)
        }
        _ => String::new(),
    };
    let envelope = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Header>{security}</s:Header><s:Body>{body}</s:Body></s:Envelope>"#
    );
    let response = client
        .post(url)
        .header(
            reqwest::header::CONTENT_TYPE,
            format!("application/soap+xml; charset=utf-8; action=\"{action}\""),
        )
        .body(envelope)
        .send()
        .await
        .map_err(|error| AppError::Upstream(format!("ONVIF request failed: {error}")))?;
    let status = response.status();
    let response_body = response
        .text()
        .await
        .map_err(|error| AppError::Upstream(format!("ONVIF response failed: {error}")))?;
    if !status.is_success() {
        return Err(AppError::Upstream(format!(
            "ONVIF returned {status}: {}",
            response_body.chars().take(500).collect::<String>()
        )));
    }
    Ok(response_body)
}

fn ws_security_header(username: &str, password: &str) -> String {
    let mut nonce = [0u8; 16];
    OsRng.fill_bytes(&mut nonce);
    let created = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let mut digest = Sha1::new();
    digest.update(nonce);
    digest.update(created.as_bytes());
    digest.update(password.as_bytes());
    let password_digest = STANDARD.encode(digest.finalize());
    let nonce_base64 = STANDARD.encode(nonce);
    format!(
        r#"<wsse:Security s:mustUnderstand="1" xmlns:wsse="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd" xmlns:wsu="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd"><wsse:UsernameToken><wsse:Username>{}</wsse:Username><wsse:Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest">{password_digest}</wsse:Password><wsse:Nonce EncodingType="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary">{nonce_base64}</wsse:Nonce><wsu:Created>{created}</wsu:Created></wsse:UsernameToken></wsse:Security>"#,
        xml_escape(username)
    )
}

fn parse_capability_urls(xml: &str) -> Result<(String, String)> {
    let document = Document::parse(xml)
        .map_err(|error| AppError::Upstream(format!("invalid ONVIF capabilities: {error}")))?;
    let mut media_url = None;
    let mut ptz_url = None;
    for node in document
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "XAddr")
    {
        let Some(value) = node.text().map(str::trim).filter(|value| !value.is_empty()) else {
            continue;
        };
        let ancestors = node
            .ancestors()
            .filter(|ancestor| ancestor.is_element())
            .map(|ancestor| ancestor.tag_name().name())
            .collect::<Vec<_>>();
        if ancestors.iter().any(|name| *name == "PTZ") {
            ptz_url = Some(value.to_string());
        } else if ancestors.iter().any(|name| *name == "Media") {
            media_url = Some(value.to_string());
        }
    }
    match (media_url, ptz_url) {
        (Some(media), Some(ptz)) => Ok((media, ptz)),
        _ => Err(AppError::Validation(
            "摄像头没有报告ONVIF媒体或PTZ服务".into(),
        )),
    }
}

fn parse_profile_token(xml: &str) -> Result<String> {
    let document = Document::parse(xml)
        .map_err(|error| AppError::Upstream(format!("invalid ONVIF profiles: {error}")))?;
    document
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "Profiles")
        .find_map(|node| node.attribute("token").map(str::to_string))
        .ok_or_else(|| AppError::Validation("摄像头没有可用的ONVIF媒体配置".into()))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
