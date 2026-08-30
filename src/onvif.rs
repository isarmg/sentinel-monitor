use crate::error::{AppError, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::{SecondsFormat, Utc};
use ipnet::IpNet;
use rand::{rngs::OsRng, RngCore};
use reqwest::{redirect::Policy, Client};
use roxmltree::{Document, ParsingOptions};
use serde::Serialize;
use sha1::{Digest, Sha1};
use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};
use tokio::{
    net::{lookup_host, UdpSocket},
    time,
};
use url::{Host, Url};
use uuid::Uuid;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_XADDRS_PER_SERVICE: usize = 8;
const MAX_XML_NODES: u32 = 4_096;
const MAX_XML_DEPTH: usize = 32;
const MAX_XML_TEXT_BYTES: usize = 64 * 1024;
const MAX_XML_TOTAL_TEXT_BYTES: usize = 256 * 1024;
const MAX_XADDR_BYTES: usize = 8 * 1024;
const MAX_PROFILE_TOKEN_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy)]
pub struct PtzCommand<'a> {
    pub action: &'a str,
    pub pan: f64,
    pub tilt: f64,
    pub zoom: f64,
}

struct ResolvedTarget {
    url: Url,
    ip: IpAddr,
}

struct AddressPolicy {
    registered_ip: IpAddr,
    xaddr_allowlist: Vec<IpNet>,
    allow_unsafe_registered: bool,
}

#[derive(Default)]
struct CapabilityUrls {
    media: Vec<String>,
    ptz: Vec<String>,
    events: Vec<String>,
}

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
    device_service_url: &str,
    username: Option<&str>,
    password: Option<&str>,
    command: PtzCommand<'_>,
    xaddr_allowlist: &[IpNet],
) -> Result<()> {
    time::timeout(
        OPERATION_TIMEOUT,
        ptz_with_policy(
            device_service_url,
            username,
            password,
            command,
            xaddr_allowlist,
            false,
        ),
    )
    .await
    .map_err(|_| AppError::Upstream("ONVIF操作超过总超时".into()))?
}

pub fn validate_configured_url(raw_url: &str) -> Result<()> {
    let url = parse_http_url(raw_url, false)?;
    let literal_ip = match url.host().expect("URL host was validated") {
        Host::Ipv4(ip) => Some(IpAddr::V4(ip)),
        Host::Ipv6(ip) => Some(IpAddr::V6(ip)),
        Host::Domain(_) => None,
    };
    if literal_ip.is_some_and(is_unsafe_address) {
        return Err(AppError::Validation("ONVIF目标地址不安全".into()));
    }
    Ok(())
}

async fn ptz_with_policy(
    device_service_url: &str,
    username: Option<&str>,
    password: Option<&str>,
    command: PtzCommand<'_>,
    xaddr_allowlist: &[IpNet],
    allow_unsafe_registered: bool,
) -> Result<()> {
    let (policy, device_target) = AddressPolicy::for_registered_target(
        device_service_url,
        xaddr_allowlist,
        allow_unsafe_registered,
    )
    .await?;
    let capabilities_body = r#"<tds:GetCapabilities xmlns:tds="http://www.onvif.org/ver10/device/wsdl"><tds:Category>All</tds:Category></tds:GetCapabilities>"#;
    let capabilities = soap_request(
        &device_target,
        "http://www.onvif.org/ver10/device/wsdl/GetCapabilities",
        capabilities_body,
        username,
        password,
    )
    .await?;
    let capability_urls = parse_capability_urls(&capabilities)?;
    let (media_target, ptz_target) = resolve_capability_targets(&policy, capability_urls).await?;

    let profiles = soap_request(
        &media_target,
        "http://www.onvif.org/ver10/media/wsdl/GetProfiles",
        r#"<trt:GetProfiles xmlns:trt="http://www.onvif.org/ver10/media/wsdl"/>"#,
        username,
        password,
    )
    .await?;
    let profile_token = parse_profile_token(&profiles)?;
    let token = xml_escape(&profile_token);

    let body = if command.action == "stop" {
        format!(
            r#"<tptz:Stop xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl"><tptz:ProfileToken>{token}</tptz:ProfileToken><tptz:PanTilt>true</tptz:PanTilt><tptz:Zoom>true</tptz:Zoom></tptz:Stop>"#
        )
    } else {
        format!(
            r#"<tptz:ContinuousMove xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema"><tptz:ProfileToken>{token}</tptz:ProfileToken><tptz:Velocity><tt:PanTilt x="{}" y="{}"/><tt:Zoom x="{}"/></tptz:Velocity><tptz:Timeout>PT1S</tptz:Timeout></tptz:ContinuousMove>"#,
            command.pan, command.tilt, command.zoom
        )
    };
    let soap_action = if command.action == "stop" {
        "http://www.onvif.org/ver20/ptz/wsdl/Stop"
    } else {
        "http://www.onvif.org/ver20/ptz/wsdl/ContinuousMove"
    };
    soap_request(&ptz_target, soap_action, &body, username, password).await?;
    Ok(())
}

impl AddressPolicy {
    async fn for_registered_target(
        raw_url: &str,
        xaddr_allowlist: &[IpNet],
        allow_unsafe_registered: bool,
    ) -> Result<(Self, ResolvedTarget)> {
        let url = parse_http_url(raw_url, false)?;
        let ips = resolve_ips(&url, false).await?;
        let ip = select_registered_ip(&ips, allow_unsafe_registered)?;
        Ok((
            Self {
                registered_ip: ip,
                xaddr_allowlist: xaddr_allowlist.to_vec(),
                allow_unsafe_registered,
            },
            ResolvedTarget { url, ip },
        ))
    }

    async fn resolve_reported_target(&self, raw_url: &str) -> Result<ResolvedTarget> {
        let url = parse_http_url(raw_url, true)?;
        let ips = resolve_ips(&url, true).await?;
        let ip = self.approve_reported_ips(&ips)?;
        Ok(ResolvedTarget { url, ip })
    }

    fn approve_reported_ips(&self, ips: &[IpAddr]) -> Result<IpAddr> {
        for ip in ips {
            let is_test_registered = self.allow_unsafe_registered && self.registered_ip == *ip;
            if is_unsafe_address(*ip) && !is_test_registered {
                return Err(AppError::Upstream("ONVIF设备报告了不安全的服务地址".into()));
            }
            let approved = self.registered_ip == *ip
                || self
                    .xaddr_allowlist
                    .iter()
                    .any(|network| network.contains(ip));
            if !approved {
                return Err(AppError::Upstream(
                    "ONVIF设备报告了未授权的跨地址服务".into(),
                ));
            }
        }
        ips.first()
            .copied()
            .ok_or_else(|| AppError::Upstream("ONVIF设备报告的服务地址无法解析".into()))
    }
}

fn select_registered_ip(ips: &[IpAddr], allow_unsafe_registered: bool) -> Result<IpAddr> {
    if ips
        .iter()
        .any(|ip| is_unsafe_address(*ip) && !allow_unsafe_registered)
    {
        return Err(AppError::Validation("ONVIF目标地址不安全".into()));
    }
    ips.first()
        .copied()
        .ok_or_else(|| AppError::Validation("ONVIF目标无法解析".into()))
}

async fn resolve_capability_targets(
    policy: &AddressPolicy,
    capability_urls: CapabilityUrls,
) -> Result<(ResolvedTarget, ResolvedTarget)> {
    let mut media_targets = Vec::with_capacity(capability_urls.media.len());
    for url in capability_urls.media {
        media_targets.push(policy.resolve_reported_target(&url).await?);
    }
    let mut ptz_targets = Vec::with_capacity(capability_urls.ptz.len());
    for url in capability_urls.ptz {
        ptz_targets.push(policy.resolve_reported_target(&url).await?);
    }
    for url in capability_urls.events {
        policy.resolve_reported_target(&url).await?;
    }

    match (
        media_targets.into_iter().next(),
        ptz_targets.into_iter().next(),
    ) {
        (Some(media), Some(ptz)) => Ok((media, ptz)),
        _ => Err(AppError::Upstream(
            "ONVIF设备没有报告媒体或PTZ服务地址".into(),
        )),
    }
}

fn parse_http_url(raw_url: &str, reported: bool) -> Result<Url> {
    let invalid = || {
        if reported {
            AppError::Upstream("ONVIF设备报告了无效的服务地址".into())
        } else {
            AppError::Validation("ONVIF目标地址无效".into())
        }
    };
    let url = Url::parse(raw_url).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || raw_has_userinfo(raw_url)
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(invalid());
    }
    Ok(url)
}

fn raw_has_userinfo(raw_url: &str) -> bool {
    raw_url
        .find("://")
        .and_then(|index| raw_url[index + 3..].split(['/', '?', '#']).next())
        .is_some_and(|authority| authority.contains('@'))
}

async fn resolve_ips(url: &Url, reported: bool) -> Result<Vec<IpAddr>> {
    let port = url.port_or_known_default().ok_or_else(|| {
        if reported {
            AppError::Upstream("ONVIF设备报告了无效的服务端口".into())
        } else {
            AppError::Validation("ONVIF目标端口无效".into())
        }
    })?;
    let mut ips = match url.host().expect("URL host was validated") {
        Host::Ipv4(ip) => vec![IpAddr::V4(ip)],
        Host::Ipv6(ip) => vec![IpAddr::V6(ip)],
        Host::Domain(domain) => time::timeout(CONNECT_TIMEOUT, lookup_host((domain, port)))
            .await
            .map_err(|_| resolution_error(reported))?
            .map_err(|_| resolution_error(reported))?
            .map(|address| address.ip())
            .collect(),
    };
    ips.sort_unstable();
    ips.dedup();
    if ips.is_empty() {
        return Err(if reported {
            AppError::Upstream("ONVIF设备报告的服务地址无法解析".into())
        } else {
            AppError::Validation("ONVIF目标无法解析".into())
        });
    }
    Ok(ips)
}

fn resolution_error(reported: bool) -> AppError {
    if reported {
        AppError::Upstream("ONVIF设备报告的服务地址无法解析".into())
    } else {
        AppError::Validation("ONVIF目标无法解析".into())
    }
}

fn is_unsafe_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_unsafe_ipv4(ip),
        IpAddr::V6(ip) => {
            ip.is_unspecified()
                || ip.is_loopback()
                || ip.is_unicast_link_local()
                || ip.is_multicast()
                || ip == Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254)
                || ip.to_ipv4_mapped().is_some_and(is_unsafe_ipv4)
        }
    }
}

fn is_unsafe_ipv4(ip: Ipv4Addr) -> bool {
    ip.octets()[0] == 0
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.octets() == [255, 255, 255, 255]
        || matches!(
            ip.octets(),
            [100, 100, 100, 200] | [168, 63, 129, 16] | [192, 0, 0, 192] | [192, 0, 0, 8]
        )
}

fn parse_probe_response(bytes: &[u8], remote: SocketAddr) -> Option<DiscoveredDevice> {
    let xml = std::str::from_utf8(bytes).ok()?;
    let document = parse_bounded_xml(xml, "发现响应").ok()?;
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
    target: &ResolvedTarget,
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
    let client = client_for_target(target)?;
    let mut response = client
        .post(target.url.clone())
        .header(
            reqwest::header::CONTENT_TYPE,
            format!("application/soap+xml; charset=utf-8; action=\"{action}\""),
        )
        .body(envelope)
        .send()
        .await
        .map_err(safe_http_error)?;
    let status = response.status();
    if !status.is_success() {
        return Err(AppError::Upstream(format!("ONVIF返回了HTTP {status}")));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(AppError::Upstream("ONVIF响应超过大小限制".into()));
    }
    let mut response_body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(safe_http_error)? {
        if response_body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(AppError::Upstream("ONVIF响应超过大小限制".into()));
        }
        response_body.extend_from_slice(&chunk);
    }
    let response_body = String::from_utf8(response_body)
        .map_err(|_| AppError::Upstream("ONVIF响应不是有效的UTF-8".into()))?;
    parse_bounded_xml(&response_body, "SOAP响应")?;
    Ok(response_body)
}

fn client_for_target(target: &ResolvedTarget) -> Result<Client> {
    let host = target
        .url
        .host_str()
        .ok_or_else(|| AppError::Upstream("ONVIF服务地址缺少主机".into()))?;
    let port = target
        .url
        .port_or_known_default()
        .ok_or_else(|| AppError::Upstream("ONVIF服务地址缺少端口".into()))?;
    Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .resolve(host, SocketAddr::new(target.ip, port))
        .build()
        .map_err(|_| AppError::Internal("ONVIF HTTP客户端初始化失败".into()))
}

fn safe_http_error(error: reqwest::Error) -> AppError {
    let message = if error.is_timeout() {
        "ONVIF请求超时"
    } else if error.is_connect() {
        "ONVIF连接失败"
    } else {
        "ONVIF请求失败"
    };
    AppError::Upstream(message.into())
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

fn parse_capability_urls(xml: &str) -> Result<CapabilityUrls> {
    let document = parse_bounded_xml(xml, "能力响应")?;
    let mut urls = CapabilityUrls::default();
    for node in document.descendants().filter(|node| node.is_element()) {
        let name = node.tag_name().name();
        if matches!(name, "Media" | "PTZ" | "Events") {
            if let Some(value) = node
                .attributes()
                .find(|attribute| attribute.name() == "XAddr")
                .map(|attribute| attribute.value())
            {
                let destination = match name {
                    "Media" => &mut urls.media,
                    "PTZ" => &mut urls.ptz,
                    "Events" => &mut urls.events,
                    _ => unreachable!(),
                };
                extend_capability_urls(destination, value)?;
            }
        }
        if name != "XAddr" {
            continue;
        }
        let Some(value) = node.text() else {
            continue;
        };
        let service = node
            .ancestors()
            .filter(|ancestor| ancestor.is_element())
            .map(|ancestor| ancestor.tag_name().name())
            .find(|name| matches!(*name, "Media" | "PTZ" | "Events"));
        let Some(service) = service else {
            continue;
        };
        let destination = match service {
            "Media" => &mut urls.media,
            "PTZ" => &mut urls.ptz,
            "Events" => &mut urls.events,
            _ => unreachable!(),
        };
        extend_capability_urls(destination, value)?;
    }
    if urls.media.is_empty() || urls.ptz.is_empty() {
        Err(AppError::Upstream(
            "ONVIF设备没有报告媒体或PTZ服务地址".into(),
        ))
    } else {
        Ok(urls)
    }
}

fn extend_capability_urls(destination: &mut Vec<String>, value: &str) -> Result<()> {
    for value in value.split_whitespace() {
        if value.len() > MAX_XADDR_BYTES {
            return Err(AppError::Upstream("ONVIF设备报告的服务地址过大".into()));
        }
        destination.push(value.to_string());
        if destination.len() > MAX_XADDRS_PER_SERVICE {
            return Err(AppError::Upstream("ONVIF设备报告了过多服务地址".into()));
        }
    }
    Ok(())
}

fn parse_profile_token(xml: &str) -> Result<String> {
    let document = parse_bounded_xml(xml, "媒体配置响应")?;
    let token = document
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "Profiles")
        .find_map(|node| node.attribute("token").map(str::to_string))
        .ok_or_else(|| AppError::Validation("摄像头没有可用的ONVIF媒体配置".into()))?;
    if token.len() > MAX_PROFILE_TOKEN_BYTES {
        return Err(AppError::Upstream("ONVIF媒体配置令牌过大".into()));
    }
    Ok(token)
}

fn parse_bounded_xml<'a>(xml: &'a str, context: &str) -> Result<Document<'a>> {
    if xml.len() > MAX_RESPONSE_BYTES {
        return Err(AppError::Upstream("ONVIF XML超过大小限制".into()));
    }
    if contains_ascii_case(xml.as_bytes(), b"<!DOCTYPE")
        || contains_ascii_case(xml.as_bytes(), b"<!ENTITY")
    {
        return Err(AppError::Upstream("ONVIF XML禁止DTD和实体声明".into()));
    }
    let document = Document::parse_with_options(
        xml,
        ParsingOptions {
            allow_dtd: false,
            nodes_limit: MAX_XML_NODES,
        },
    )
    .map_err(|_| AppError::Upstream(format!("ONVIF{context}XML无效")))?;

    let mut total_text_bytes = 0usize;
    for node in document.descendants() {
        let depth = node
            .ancestors()
            .filter(|ancestor| ancestor.is_element())
            .count();
        if depth > MAX_XML_DEPTH {
            return Err(AppError::Upstream("ONVIF XML嵌套过深".into()));
        }
        if node.is_text() {
            let length = node.text().unwrap_or_default().len();
            if length > MAX_XML_TEXT_BYTES {
                return Err(AppError::Upstream("ONVIF XML文本节点过大".into()));
            }
            total_text_bytes = total_text_bytes.saturating_add(length);
            if total_text_bytes > MAX_XML_TOTAL_TEXT_BYTES {
                return Err(AppError::Upstream("ONVIF XML文本总量过大".into()));
            }
        }
    }
    Ok(document)
}

fn contains_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, Bytes},
        extract::State,
        http::{header::LOCATION, StatusCode},
        response::Response,
        routing::post,
        Router,
    };
    use std::{
        convert::Infallible,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };

    #[derive(Clone)]
    struct MaliciousState {
        base_url: String,
        followed_redirects: Arc<AtomicUsize>,
    }

    #[tokio::test]
    async fn address_policy_rejects_unsafe_targets_and_requires_cross_address_allowlist() {
        for url in [
            "file:///etc/passwd",
            "gopher://192.168.1.20/1",
            "ftp://192.168.1.20/onvif",
            "http://admin:secret@192.168.1.20/onvif",
            "http://@192.168.1.20/onvif",
            "http://127.0.0.1/onvif",
            "http://2130706433/onvif",
            "http://0x7f000001/onvif",
            "http://169.254.169.254/latest/meta-data",
            "http://0.0.0.0/onvif",
            "http://0.0.0.1/onvif",
            "http://224.0.0.1/onvif",
            "http://100.100.100.200/latest/meta-data",
            "http://168.63.129.16/onvif",
            "http://[::1]/onvif",
            "http://[::]/onvif",
            "http://[::ffff:127.0.0.1]/onvif",
            "http://[fe80::1]/onvif",
            "http://[ff02::1]/onvif",
            "http://[fd00:ec2::254]/latest/meta-data",
        ] {
            assert!(
                AddressPolicy::for_registered_target(url, &[], false)
                    .await
                    .is_err(),
                "unsafe target was accepted: {url}"
            );
        }

        let (same_ip_policy, _) = AddressPolicy::for_registered_target(
            "http://192.168.1.20/onvif/device_service",
            &[],
            false,
        )
        .await
        .expect("accept private registered camera");
        same_ip_policy
            .resolve_reported_target("http://192.168.1.20:8080/onvif/media")
            .await
            .expect("accept same resolved IP on another port");
        assert!(same_ip_policy
            .resolve_reported_target("http://192.168.1.21/onvif/media")
            .await
            .is_err());
        let mixed_registered_ips = [
            "192.168.1.20".parse::<IpAddr>().unwrap(),
            "127.0.0.1".parse::<IpAddr>().unwrap(),
        ];
        assert!(select_registered_ip(&mixed_registered_ips, false).is_err());
        assert!(same_ip_policy
            .approve_reported_ips(&mixed_registered_ips)
            .is_err());

        let allowlist = ["192.168.2.0/24".parse::<IpNet>().unwrap()];
        let (allowlisted_policy, _) = AddressPolicy::for_registered_target(
            "http://192.168.1.20/onvif/device_service",
            &allowlist,
            false,
        )
        .await
        .expect("accept private registered camera");
        allowlisted_policy
            .resolve_reported_target("https://192.168.2.9/onvif/ptz")
            .await
            .expect("accept explicitly allowlisted cross address");

        let broad_allowlist = ["0.0.0.0/0".parse::<IpNet>().unwrap()];
        let (broad_policy, _) = AddressPolicy::for_registered_target(
            "http://192.168.1.20/onvif/device_service",
            &broad_allowlist,
            false,
        )
        .await
        .expect("accept private registered camera");
        assert!(broad_policy
            .resolve_reported_target("http://127.0.0.1/onvif/ptz")
            .await
            .is_err());
    }

    #[test]
    fn xml_parser_enforces_dtd_node_depth_and_text_budgets() {
        let dtd =
            r#"<!DOCTYPE Envelope [<!ENTITY secret "expanded">]><Envelope>&secret;</Envelope>"#;
        assert!(parse_bounded_xml(dtd, "测试").is_err());

        let deep = format!(
            "{}content{}",
            "<node>".repeat(MAX_XML_DEPTH + 1),
            "</node>".repeat(MAX_XML_DEPTH + 1)
        );
        assert!(parse_bounded_xml(&deep, "测试").is_err());

        let many_nodes = format!("<root>{}</root>", "<node/>".repeat(MAX_XML_NODES as usize));
        assert!(parse_bounded_xml(&many_nodes, "测试").is_err());

        let large_text = format!(
            "<root>{}</root>",
            "x".repeat(MAX_XML_TEXT_BYTES.saturating_add(1))
        );
        assert!(parse_bounded_xml(&large_text, "测试").is_err());

        let text_chunk = "x".repeat(60_000);
        let excessive_total = format!(
            "<root>{}</root>",
            (0..5)
                .map(|_| format!("<node>{text_chunk}</node>"))
                .collect::<String>()
        );
        assert!(parse_bounded_xml(&excessive_total, "测试").is_err());

        let too_many_xaddrs = format!(
            "<Envelope><Media>{}</Media><PTZ><XAddr>http://192.168.1.20/ptz</XAddr></PTZ></Envelope>",
            (0..=MAX_XADDRS_PER_SERVICE)
                .map(|index| format!(
                    "<XAddr>http://192.168.1.20/media/{index}</XAddr>"
                ))
                .collect::<String>()
        );
        assert!(parse_capability_urls(&too_many_xaddrs).is_err());

        let attribute_xaddrs = r#"<Envelope><Capabilities><Media XAddr="http://192.168.1.20/media"/><PTZ XAddr="http://192.168.1.20/ptz"/><Events XAddr="http://192.168.1.20/events"/></Capabilities></Envelope>"#;
        let parsed =
            parse_capability_urls(attribute_xaddrs).expect("parse standard XAddr attributes");
        assert_eq!(parsed.media, ["http://192.168.1.20/media"]);
        assert_eq!(parsed.ptz, ["http://192.168.1.20/ptz"]);
        assert_eq!(parsed.events, ["http://192.168.1.20/events"]);

        let oversized_profile_token = format!(
            "<Envelope><Profiles token=\"{}\"/></Envelope>",
            "x".repeat(MAX_PROFILE_TOKEN_BYTES + 1)
        );
        assert!(parse_profile_token(&oversized_profile_token).is_err());
    }

    #[tokio::test]
    async fn local_malicious_onvif_responses_are_bounded_and_never_redirected() {
        let _network_guard = crate::NETWORK_TEST_LOCK.lock().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind malicious ONVIF server");
        let address = listener.local_addr().expect("malicious server address");
        let base_url = format!("http://{address}");
        let followed_redirects = Arc::new(AtomicUsize::new(0));
        let state = MaliciousState {
            base_url: base_url.clone(),
            followed_redirects: followed_redirects.clone(),
        };
        let app = Router::new()
            .route("/cross-host", post(cross_host_capabilities))
            .route("/cross-events", post(cross_events_capabilities))
            .route("/redirect", post(redirect_response))
            .route("/followed", post(followed_response))
            .route("/pinned", post(pinned_response))
            .route("/error-body", post(error_body_response))
            .route("/oversized", post(oversized_response))
            .route("/oversized-chunked", post(oversized_chunked_response))
            .route("/xml-bomb", post(xml_bomb_response))
            .route("/malformed-secret", post(malformed_secret_response))
            .with_state(state);
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve malicious ONVIF responses");
        });

        let pinned_target = ResolvedTarget {
            url: Url::parse(&format!(
                "http://validated-camera.invalid:{}/pinned",
                address.port()
            ))
            .unwrap(),
            ip: address.ip(),
        };
        soap_request(
            &pinned_target,
            "urn:test:pinned-resolution",
            "<Test/>",
            None,
            None,
        )
        .await
        .expect("validated IP must be pinned instead of resolving the URL again");

        let command = PtzCommand {
            action: "stop",
            pan: 0.0,
            tilt: 0.0,
            zoom: 0.0,
        };
        for (path, expected_error) in [
            ("cross-host", "跨地址"),
            ("cross-events", "跨地址"),
            ("redirect", "HTTP"),
            ("error-body", "HTTP"),
            ("oversized", "大小"),
            ("oversized-chunked", "大小"),
            ("xml-bomb", "DTD"),
            ("malformed-secret", "XML无效"),
        ] {
            let error = ptz_with_policy(
                &format!("{base_url}/{path}"),
                Some("sensitive-user"),
                Some("sensitive-password"),
                command,
                &[],
                true,
            )
            .await
            .expect_err("malicious ONVIF response must be rejected");
            let message = error.to_string();
            assert!(
                message.contains(expected_error),
                "unexpected error: {message}"
            );
            assert!(!message.contains("sensitive-user"));
            assert!(!message.contains("sensitive-password"));
        }
        assert_eq!(followed_redirects.load(Ordering::SeqCst), 0);
        server.abort();
        let _ = server.await;
    }

    async fn cross_host_capabilities(State(state): State<MaliciousState>) -> Response {
        let xml = format!(
            r#"<Envelope><Body><Capabilities><Media XAddr="http://192.0.2.55/media"/><PTZ XAddr="{}/ptz"/><Events XAddr="{}/events"/></Capabilities></Body></Envelope>"#,
            state.base_url, state.base_url
        );
        Response::new(Body::from(xml))
    }

    async fn cross_events_capabilities(State(state): State<MaliciousState>) -> Response {
        let xml = format!(
            r#"<Envelope><Body><Capabilities><Media><XAddr>{}/media</XAddr></Media><PTZ><XAddr>{}/ptz</XAddr></PTZ><Events><XAddr>http://192.0.2.55/events</XAddr></Events></Capabilities></Body></Envelope>"#,
            state.base_url, state.base_url
        );
        Response::new(Body::from(xml))
    }

    async fn redirect_response() -> Response {
        Response::builder()
            .status(StatusCode::FOUND)
            .header(LOCATION, "/followed")
            .body(Body::empty())
            .expect("build redirect response")
    }

    async fn followed_response(State(state): State<MaliciousState>) -> Response {
        state.followed_redirects.fetch_add(1, Ordering::SeqCst);
        Response::new(Body::from("<Envelope/>"))
    }

    async fn error_body_response() -> Response {
        Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from("sensitive-user sensitive-password"))
            .expect("build error response")
    }

    async fn pinned_response() -> Response {
        Response::new(Body::from("<Envelope/>"))
    }

    async fn oversized_response() -> Response {
        Response::new(Body::from(vec![b'x'; MAX_RESPONSE_BYTES + 1]))
    }

    async fn oversized_chunked_response() -> Response {
        let chunks =
            (0..17).map(|_| Ok::<_, Infallible>(Bytes::from(vec![b'x'; MAX_RESPONSE_BYTES / 16])));
        Response::new(Body::from_stream(futures_util::stream::iter(chunks)))
    }

    async fn xml_bomb_response() -> Response {
        Response::new(Body::from(
            r#"<!DOCTYPE Envelope [<!ENTITY bomb "expanded">]><Envelope><Body>&bomb;</Body></Envelope>"#,
        ))
    }

    async fn malformed_secret_response() -> Response {
        Response::new(Body::from(
            "<Envelope><Body>&sensitive-user;</Body></Envelope>",
        ))
    }
}
