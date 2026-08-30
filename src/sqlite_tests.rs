use crate::{
    auth::{
        bootstrap_admin, csrf_cookie, csrf_cookie_name, issue_media_token, issue_session,
        session_cookie, session_cookie_name, IssuedBrowserSession,
    },
    background::emit_event,
    config::Config,
    crypto::SecretBox,
    mediamtx::MediaMtxClient,
    models::CameraRecord,
    routes, sqlite, AppState,
};
use axum::{
    body::{to_bytes, Body},
    http::{
        header::{AUTHORIZATION, COOKIE, HOST, ORIGIN, SET_COOKIE},
        HeaderMap, Method, Request, StatusCode,
    },
    response::Response,
    Router,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::{SqliteConnection, SqlitePool};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{sync::broadcast, task::JoinHandle};
use tower::ServiceExt;
use uuid::Uuid;

struct TestContext {
    _database: TemporaryDatabase,
    state: AppState,
    media_task: JoinHandle<()>,
    _network_guard: tokio::sync::MutexGuard<'static, ()>,
}

#[derive(Clone)]
struct BrowserCredentials {
    session_id: Option<Uuid>,
    token: String,
    csrf_token: String,
}

impl From<IssuedBrowserSession> for BrowserCredentials {
    fn from(session: IssuedBrowserSession) -> Self {
        Self {
            session_id: Some(session.session_id),
            token: session.token,
            csrf_token: session.csrf_token,
        }
    }
}

impl BrowserCredentials {
    fn cookie_header(&self) -> String {
        format!(
            "sentinel_session={}; sentinel_csrf={}",
            self.token, self.csrf_token
        )
    }
}

#[derive(Default)]
struct BrowserRequestContext<'a> {
    credentials: Option<&'a BrowserCredentials>,
    csrf_token: Option<&'a str>,
    host: Option<&'a str>,
    origin: Option<&'a str>,
}

impl Drop for TestContext {
    fn drop(&mut self) {
        self.media_task.abort();
    }
}

struct TemporaryDatabase {
    path: PathBuf,
}

impl TemporaryDatabase {
    fn new() -> Self {
        Self {
            path: std::env::temp_dir().join(format!(
                "sentinel-monitor-sqlite-test-{}.sqlite3",
                Uuid::new_v4()
            )),
        }
    }

    fn url(&self) -> String {
        format!("sqlite://{}", self.path.display())
    }
}

impl Drop for TemporaryDatabase {
    fn drop(&mut self) {
        let base = self.path.to_string_lossy();
        for path in [
            self.path.clone(),
            PathBuf::from(format!("{base}-shm")),
            PathBuf::from(format!("{base}-wal")),
        ] {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl TestContext {
    async fn migrated() -> Self {
        let network_guard = crate::NETWORK_TEST_LOCK.lock().await;
        let database = TemporaryDatabase::new();
        let database_url = database.url();
        let pool = sqlite::open_pool(&database_url)
            .await
            .expect("open temporary SQLite database");
        sqlx::migrate!()
            .run(&pool)
            .await
            .expect("run the real migrations");

        let media_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake media service");
        let media_address = media_listener.local_addr().expect("fake media address");
        let media_task = tokio::spawn(async move {
            let service = Router::new().fallback(|| async { StatusCode::OK });
            axum::serve(media_listener, service)
                .await
                .expect("serve fake media service");
        });
        let media_url = format!("http://{media_address}");

        let config = Arc::new(Config {
            bind_addr: "127.0.0.1:0".parse().expect("test bind address"),
            database_url,
            jwt_secret: b"sentinel-test-jwt-secret-32-bytes".to_vec(),
            credentials_key: [7; 32],
            bootstrap_admin_email: "admin@example.com".into(),
            bootstrap_admin_password: Some("bootstrap-password".into()),
            development_mode: true,
            session_idle_ttl: Duration::from_secs(1_800),
            session_absolute_ttl: Duration::from_secs(3_600),
            media_token_ttl: Duration::from_secs(120),
            mediamtx_api_url: media_url.clone(),
            mediamtx_playback_url: media_url,
            public_webrtc_base_url: "/media-webrtc".into(),
            public_hls_base_url: "/media-hls".into(),
            status_interval: Duration::from_secs(10),
            reconcile_interval: Duration::from_secs(60),
            request_timeout: Duration::from_millis(100),
            onvif_discovery_timeout: Duration::from_millis(100),
            onvif_xaddr_allowlist: Vec::new(),
            static_dir: PathBuf::from("web/dist"),
        });
        let http = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .build()
            .expect("build test HTTP client");
        let media = MediaMtxClient::new(
            http.clone(),
            config.mediamtx_api_url.clone(),
            config.mediamtx_playback_url.clone(),
        );
        let (events, _) = broadcast::channel(16);
        let state = AppState {
            config: config.clone(),
            pool,
            secrets: SecretBox::new(&config.credentials_key),
            http,
            media,
            events,
        };

        Self {
            _database: database,
            state,
            media_task,
            _network_guard: network_guard,
        }
    }

    async fn bootstrap(&self) -> (Uuid, BrowserCredentials) {
        bootstrap_admin(&self.state)
            .await
            .expect("bootstrap administrator");
        let (id, session_version) = sqlx::query_as::<_, (Uuid, i64)>(
            "SELECT id, session_version FROM users WHERE role = 'admin'",
        )
        .fetch_one(&self.state.pool)
        .await
        .expect("load administrator id");
        let session = issue_session(&self.state, id, session_version)
            .await
            .expect("issue administrator session");
        (id, session.into())
    }
}

async fn send_json(
    app: &Router,
    method: Method,
    uri: &str,
    credentials: &BrowserCredentials,
    value: serde_json::Value,
) -> StatusCode {
    send_request(app, method, uri, credentials, Some(value))
        .await
        .status()
}

async fn send_request(
    app: &Router,
    method: Method,
    uri: &str,
    credentials: &BrowserCredentials,
    value: Option<Value>,
) -> Response {
    send_custom_request(
        app,
        method,
        uri,
        BrowserRequestContext {
            credentials: Some(credentials),
            csrf_token: Some(&credentials.csrf_token),
            host: Some("sentinel.test"),
            origin: Some("https://sentinel.test"),
        },
        value,
    )
    .await
}

async fn send_custom_request(
    app: &Router,
    method: Method,
    uri: &str,
    context: BrowserRequestContext<'_>,
    value: Option<Value>,
) -> Response {
    let body = value
        .map(|value| Body::from(value.to_string()))
        .unwrap_or_else(Body::empty);
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(host) = context.host {
        builder = builder.header(HOST, host);
    }
    if let Some(origin) = context.origin {
        builder = builder.header(ORIGIN, origin);
    }
    if let Some(credentials) = context.credentials {
        builder = builder.header(COOKIE, credentials.cookie_header());
    }
    if let Some(csrf_token) = context.csrf_token {
        builder = builder.header("x-csrf-token", csrf_token);
    }
    let request = builder
        .header("content-type", "application/json")
        .body(body)
        .expect("build test request");
    app.clone()
        .oneshot(request)
        .await
        .expect("serve test request")
}

fn login_credentials(headers: &HeaderMap, config: &Config) -> BrowserCredentials {
    BrowserCredentials {
        session_id: None,
        token: set_cookie_value(headers, session_cookie_name(config)),
        csrf_token: set_cookie_value(headers, csrf_cookie_name(config)),
    }
}

fn set_cookie_value(headers: &HeaderMap, name: &str) -> String {
    headers
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|cookie| {
            let value = cookie.strip_prefix(name)?.strip_prefix('=')?;
            Some(value.split(';').next().unwrap_or_default().to_string())
        })
        .unwrap_or_else(|| panic!("missing {name} Set-Cookie header"))
}

async fn response_json(response: Response) -> Value {
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("read JSON response body");
    serde_json::from_slice(&bytes).expect("decode JSON response body")
}

async fn required_times(pool: &SqlitePool, table: &str, id: Uuid) -> (String, Option<String>) {
    let sql = match table {
        "users" | "cameras" => {
            format!("SELECT created_at, updated_at FROM {table} WHERE id = ?")
        }
        "events" | "audit_logs" => {
            format!("SELECT created_at, NULL FROM {table} WHERE id = ?")
        }
        _ => panic!("unsupported table"),
    };
    sqlx::query_as(&sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("load required timestamps")
}

fn assert_utc_timestamp(value: &str) {
    DateTime::parse_from_rfc3339(value).expect("timestamp must be RFC 3339");
}

async fn assert_connection_configuration(connection: &mut SqliteConnection) {
    let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&mut *connection)
        .await
        .expect("read journal_mode");
    let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(&mut *connection)
        .await
        .expect("read foreign_keys");
    let busy_timeout: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
        .fetch_one(&mut *connection)
        .await
        .expect("read busy_timeout");
    let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous")
        .fetch_one(&mut *connection)
        .await
        .expect("read synchronous");

    assert_eq!(journal_mode, "wal");
    assert_eq!(foreign_keys, 1);
    assert_eq!(busy_timeout, 5_000);
    assert_eq!(synchronous, 2);
}

async fn assert_foreign_key_enforced(connection: &mut SqliteConnection) {
    let error = sqlx::query(
        "INSERT INTO audit_logs (id, user_id, action, entity_type, details, created_at) \
         VALUES (?, ?, 'test.invalid-reference', 'test', '{}', ?)",
    )
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .bind(Utc::now())
    .execute(&mut *connection)
    .await
    .expect_err("foreign key violation must reject the insert");

    match error {
        sqlx::Error::Database(error) => assert!(error.is_foreign_key_violation(), "{error}"),
        other => panic!("expected a database constraint error, got {other}"),
    }
}

async fn assert_database_integrity(pool: &SqlitePool) {
    let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(pool)
        .await
        .expect("run integrity_check");
    let foreign_key_violations = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(pool)
        .await
        .expect("run foreign_key_check");

    assert_eq!(integrity, "ok");
    assert!(foreign_key_violations.is_empty());
}

#[tokio::test]
async fn migrated_database_accepts_all_required_creation_timestamps() {
    let context = TestContext::migrated().await;
    let (admin_id, token) = context.bootstrap().await;
    let app = routes::router(context.state.clone());

    let (created_at, updated_at) = required_times(&context.state.pool, "users", admin_id).await;
    assert_utc_timestamp(&created_at);
    assert_utc_timestamp(updated_at.as_deref().expect("administrator updated_at"));

    assert_eq!(
        send_json(
            &app,
            Method::POST,
            "/api/users",
            &token,
            json!({
                "email": "operator@example.com",
                "password": "operator-password",
                "role": "operator"
            }),
        )
        .await,
        StatusCode::CREATED
    );
    let user_id = sqlx::query_scalar::<_, Uuid>("SELECT id FROM users WHERE email = ?")
        .bind("operator@example.com")
        .fetch_one(&context.state.pool)
        .await
        .expect("load created user");
    let (created_at, updated_at) = required_times(&context.state.pool, "users", user_id).await;
    assert_utc_timestamp(&created_at);
    assert_utc_timestamp(updated_at.as_deref().expect("user updated_at"));

    assert_eq!(
        send_json(
            &app,
            Method::POST,
            "/api/cameras",
            &token,
            json!({
                "name": "Front Door",
                "location": "Entrance",
                "main_stream_url": "rtsp://camera.example/main",
                "enabled": true,
                "record_enabled": true
            }),
        )
        .await,
        StatusCode::CREATED
    );
    let camera_id = sqlx::query_scalar::<_, Uuid>("SELECT id FROM cameras WHERE name = ?")
        .bind("Front Door")
        .fetch_one(&context.state.pool)
        .await
        .expect("load created camera");
    let (created_at, updated_at) = required_times(&context.state.pool, "cameras", camera_id).await;
    assert_utc_timestamp(&created_at);
    assert_utc_timestamp(updated_at.as_deref().expect("camera updated_at"));

    let event = emit_event(
        &context.state,
        Some(camera_id),
        "camera.offline",
        "warning",
        "camera is offline",
        json!({ "source": "test" }),
    )
    .await
    .expect("create event");
    let (created_at, _) = required_times(&context.state.pool, "events", event.id).await;
    assert_utc_timestamp(&created_at);

    let audit_rows: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, created_at FROM audit_logs ORDER BY created_at")
            .fetch_all(&context.state.pool)
            .await
            .expect("load audit entries");
    assert_eq!(audit_rows.len(), 2);
    for (_, created_at) in audit_rows {
        assert_utc_timestamp(&created_at);
    }

    context.state.pool.close().await;
}

#[tokio::test]
async fn migrated_database_updates_cameras_filters_and_acknowledges_events() {
    let context = TestContext::migrated().await;
    let (admin_id, token) = context.bootstrap().await;
    sqlx::query(
        "UPDATE users SET active = 1, last_login_at = ?, created_at = ?, updated_at = ? WHERE id = ?",
    )
    .bind("2024-03-04T05:06:07Z")
    .bind("2024-01-02T03:04:05Z")
    .bind("2024-05-06T07:08:09Z")
    .bind(admin_id)
    .execute(&context.state.pool)
    .await
    .expect("set recognizable persisted user values");
    let app = routes::router(context.state.clone());

    let response = send_request(&app, Method::GET, "/api/me", &token, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let current_user = response_json(response).await;
    assert_eq!(current_user["id"], json!(admin_id));
    assert_eq!(current_user["active"], json!(true));
    assert_eq!(current_user["last_login_at"], json!("2024-03-04T05:06:07Z"));
    assert_eq!(current_user["created_at"], json!("2024-01-02T03:04:05Z"));
    assert_eq!(current_user["updated_at"], json!("2024-05-06T07:08:09Z"));

    for (name, stream) in [
        ("Target Camera", "rtsp://camera.example/target"),
        ("Other Camera", "rtsp://camera.example/other"),
    ] {
        assert_eq!(
            send_json(
                &app,
                Method::POST,
                "/api/cameras",
                &token,
                json!({
                    "name": name,
                    "location": "Initial",
                    "main_stream_url": stream,
                    "enabled": true,
                    "record_enabled": true
                }),
            )
            .await,
            StatusCode::CREATED
        );
    }
    let target_id = sqlx::query_scalar::<_, Uuid>("SELECT id FROM cameras WHERE name = ?")
        .bind("Target Camera")
        .fetch_one(&context.state.pool)
        .await
        .expect("load target camera");
    let other_id = sqlx::query_scalar::<_, Uuid>("SELECT id FROM cameras WHERE name = ?")
        .bind("Other Camera")
        .fetch_one(&context.state.pool)
        .await
        .expect("load other camera");
    sqlx::query("UPDATE cameras SET updated_at = ? WHERE id = ?")
        .bind("2020-01-01T00:00:00Z")
        .bind(target_id)
        .execute(&context.state.pool)
        .await
        .expect("set recognizable old camera update time");

    let response = send_request(
        &app,
        Method::PUT,
        &format!("/api/cameras/{target_id}"),
        &token,
        Some(json!({
            "name": "Updated Camera",
            "location": "Garage",
            "main_stream_url": "rtsps://camera.example/updated-main",
            "sub_stream_url": "rtsp://camera.example/updated-sub",
            "onvif_url": "https://camera.example/onvif/device_service",
            "username": "camera-user",
            "password": "camera-password",
            "enabled": false,
            "record_enabled": false
        })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = response_json(response).await;
    assert_eq!(response["media_synced"], json!(true));
    assert_eq!(response["camera"]["id"], json!(target_id));
    assert_eq!(response["camera"]["status"], json!("disabled"));

    let camera = sqlx::query_as::<_, CameraRecord>(
        "SELECT id, name, location, main_stream_url_enc, sub_stream_url_enc, onvif_url, username, password_enc, enabled, record_enabled, status, last_seen_at, created_at, updated_at FROM cameras WHERE id = ?",
    )
    .bind(target_id)
    .fetch_one(&context.state.pool)
    .await
    .expect("load updated camera");
    assert_eq!(camera.name, "Updated Camera");
    assert_eq!(camera.location, "Garage");
    assert_eq!(
        context
            .state
            .secrets
            .decrypt(&camera.main_stream_url_enc)
            .expect("decrypt main stream"),
        "rtsps://camera.example/updated-main"
    );
    assert_eq!(
        context
            .state
            .secrets
            .decrypt(
                camera
                    .sub_stream_url_enc
                    .as_deref()
                    .expect("updated sub stream"),
            )
            .expect("decrypt sub stream"),
        "rtsp://camera.example/updated-sub"
    );
    assert_eq!(
        camera.onvif_url.as_deref(),
        Some("https://camera.example/onvif/device_service")
    );
    assert_eq!(camera.username.as_deref(), Some("camera-user"));
    assert_eq!(
        context
            .state
            .secrets
            .decrypt(camera.password_enc.as_deref().expect("updated password"))
            .expect("decrypt password"),
        "camera-password"
    );
    assert!(!camera.enabled);
    assert!(!camera.record_enabled);
    assert_eq!(camera.status, "disabled");
    assert!(camera.updated_at > "2020-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap());

    let unacknowledged = emit_event(
        &context.state,
        Some(target_id),
        "motion",
        "warning",
        "unacknowledged target event",
        json!({}),
    )
    .await
    .expect("create unacknowledged target event");
    let acknowledged = emit_event(
        &context.state,
        Some(target_id),
        "motion",
        "critical",
        "acknowledged target event",
        json!({}),
    )
    .await
    .expect("create target event to acknowledge");
    let _other = emit_event(
        &context.state,
        Some(other_id),
        "motion",
        "warning",
        "other camera event",
        json!({}),
    )
    .await
    .expect("create other camera event");

    let response = send_request(
        &app,
        Method::POST,
        &format!("/api/events/{}/ack", acknowledged.id),
        &token,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let (acknowledged_at, acknowledged_by): (Option<DateTime<Utc>>, Option<Uuid>) =
        sqlx::query_as("SELECT acknowledged_at, acknowledged_by FROM events WHERE id = ?")
            .bind(acknowledged.id)
            .fetch_one(&context.state.pool)
            .await
            .expect("load event acknowledgement");
    assert!(acknowledged_at.is_some());
    assert_eq!(acknowledged_by, Some(admin_id));

    let response = send_request(
        &app,
        Method::GET,
        &format!("/api/events?camera_id={target_id}&unacknowledged=true&limit=10"),
        &token,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let filtered = response_json(response).await;
    let filtered = filtered.as_array().expect("event list response");
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0]["id"], json!(unacknowledged.id));

    let response = send_request(
        &app,
        Method::GET,
        &format!("/api/events?camera_id={target_id}&limit=10"),
        &token,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let all_target_events = response_json(response).await;
    let mut ids: Vec<String> = all_target_events
        .as_array()
        .expect("target event list response")
        .iter()
        .map(|event| event["id"].as_str().expect("event id").to_string())
        .collect();
    ids.sort();
    let mut expected = vec![unacknowledged.id.to_string(), acknowledged.id.to_string()];
    expected.sort();
    assert_eq!(ids, expected);

    context.state.pool.close().await;
}

#[tokio::test]
async fn browser_session_persists_across_reopen_and_machine_tokens_remain_separate() {
    let mut context = TestContext::migrated().await;
    bootstrap_admin(&context.state)
        .await
        .expect("bootstrap administrator");
    let admin_id = sqlx::query_scalar::<_, Uuid>("SELECT id FROM users WHERE role = 'admin'")
        .fetch_one(&context.state.pool)
        .await
        .expect("load administrator id");
    let app = routes::router(context.state.clone());
    let login_body = json!({
        "email": "admin@example.com",
        "password": "bootstrap-password"
    });

    let response = send_custom_request(
        &app,
        Method::POST,
        "/api/auth/login",
        BrowserRequestContext {
            host: Some("sentinel.test"),
            ..Default::default()
        },
        Some(login_body.clone()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = send_custom_request(
        &app,
        Method::POST,
        "/api/auth/login",
        BrowserRequestContext {
            host: Some("sentinel.test"),
            origin: Some("https://attacker.test"),
            ..Default::default()
        },
        Some(login_body.clone()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let response = send_custom_request(
        &app,
        Method::POST,
        "/api/auth/login",
        BrowserRequestContext {
            host: Some("sentinel.test"),
            origin: Some("https://sentinel.test"),
            ..Default::default()
        },
        Some(login_body),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let credentials = login_credentials(response.headers(), &context.state.config);
    assert!(!credentials.token.is_empty());
    assert!(!credentials.csrf_token.is_empty());

    let (token_digest, csrf_digest, idle_expires_at, absolute_expires_at): (
        Vec<u8>,
        Vec<u8>,
        DateTime<Utc>,
        DateTime<Utc>,
    ) = sqlx::query_as(
        "SELECT token_digest, csrf_digest, idle_expires_at, absolute_expires_at \
         FROM browser_sessions WHERE user_id = ?",
    )
    .bind(admin_id)
    .fetch_one(&context.state.pool)
    .await
    .expect("load persisted browser session");
    assert_eq!(token_digest.len(), 32);
    assert_eq!(csrf_digest.len(), 32);
    assert_ne!(token_digest, credentials.token.as_bytes());
    assert_ne!(csrf_digest, credentials.csrf_token.as_bytes());
    assert!(idle_expires_at < absolute_expires_at);

    let mut production = (*context.state.config).clone();
    production.development_mode = false;
    let production_session_cookie = session_cookie("session-token", &production);
    assert!(production_session_cookie.starts_with("__Host-sentinel_session=session-token;"));
    assert!(production_session_cookie.contains("; Secure"));
    assert!(production_session_cookie.contains("; HttpOnly"));
    assert!(production_session_cookie.contains("; SameSite=Strict"));
    let production_csrf_cookie = csrf_cookie("csrf-token", &production);
    assert!(production_csrf_cookie.starts_with("__Host-sentinel_csrf=csrf-token;"));
    assert!(production_csrf_cookie.contains("; Secure"));
    assert!(!production_csrf_cookie.contains("HttpOnly"));

    let bearer_only = Request::builder()
        .method(Method::GET)
        .uri("/api/me")
        .header(AUTHORIZATION, format!("Bearer {}", credentials.token))
        .body(Body::empty())
        .expect("build bearer-only browser request");
    let response = app
        .clone()
        .oneshot(bearer_only)
        .await
        .expect("serve bearer-only browser request");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let camera_id = Uuid::new_v4();
    let (machine_token, _) = issue_media_token(
        admin_id,
        camera_id,
        "camera/main".into(),
        vec!["read".into()],
        &context.state.config,
    )
    .expect("issue separate machine media token");
    let response = send_custom_request(
        &app,
        Method::POST,
        "/internal/media/auth",
        BrowserRequestContext::default(),
        Some(json!({
            "token": machine_token,
            "action": "read",
            "path": "camera/main"
        })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    assert_eq!(
        send_request(&app, Method::GET, "/api/me", &credentials, None)
            .await
            .status(),
        StatusCode::OK
    );
    drop(app);
    context.state.pool.close().await;
    context.state.pool = sqlite::open_pool(&context.state.config.database_url)
        .await
        .expect("reopen session database after simulated restart");
    let restarted_app = routes::router(context.state.clone());
    assert_eq!(
        send_request(&restarted_app, Method::GET, "/api/me", &credentials, None,)
            .await
            .status(),
        StatusCode::OK
    );
    context.state.pool.close().await;
}

#[tokio::test]
async fn browser_session_enforces_bound_csrf_origin_revocation_and_both_expiries() {
    let context = TestContext::migrated().await;
    let (admin_id, first) = context.bootstrap().await;
    let session_version =
        sqlx::query_scalar::<_, i64>("SELECT session_version FROM users WHERE id = ?")
            .bind(admin_id)
            .fetch_one(&context.state.pool)
            .await
            .expect("load administrator session version");
    let second: BrowserCredentials = issue_session(&context.state, admin_id, session_version)
        .await
        .expect("issue second browser session")
        .into();
    let app = routes::router(context.state.clone());

    let response = send_custom_request(
        &app,
        Method::POST,
        "/api/auth/logout",
        BrowserRequestContext {
            credentials: Some(&first),
            csrf_token: Some(&second.csrf_token),
            host: Some("sentinel.test"),
            origin: Some("https://sentinel.test"),
        },
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = send_custom_request(
        &app,
        Method::POST,
        "/api/auth/logout",
        BrowserRequestContext {
            credentials: Some(&first),
            csrf_token: Some(&first.csrf_token),
            host: Some("sentinel.test"),
            origin: Some("https://attacker.test"),
        },
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = send_custom_request(
        &app,
        Method::POST,
        "/api/auth/logout",
        BrowserRequestContext {
            credentials: Some(&first),
            csrf_token: Some(&first.csrf_token),
            host: Some("sentinel.test"),
            origin: None,
        },
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let response = send_request(&app, Method::POST, "/api/auth/logout", &first, None).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let revoked: bool =
        sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM browser_sessions WHERE id = ?")
            .bind(first.session_id.expect("first session id"))
            .fetch_one(&context.state.pool)
            .await
            .expect("load revoked session state");
    assert!(revoked);
    assert_eq!(
        send_request(&app, Method::GET, "/api/me", &first, None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );

    let past = Utc::now() - chrono::Duration::minutes(1);
    sqlx::query("UPDATE browser_sessions SET idle_expires_at = ? WHERE id = ?")
        .bind(past)
        .bind(second.session_id.expect("second session id"))
        .execute(&context.state.pool)
        .await
        .expect("expire idle session deadline");
    assert_eq!(
        send_request(&app, Method::GET, "/api/me", &second, None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );

    let absolute: BrowserCredentials = issue_session(&context.state, admin_id, session_version)
        .await
        .expect("issue absolute-expiry session")
        .into();
    sqlx::query(
        "UPDATE browser_sessions SET idle_expires_at = ?, absolute_expires_at = ? WHERE id = ?",
    )
    .bind(past)
    .bind(past)
    .bind(absolute.session_id.expect("absolute-expiry session id"))
    .execute(&context.state.pool)
    .await
    .expect("expire absolute session deadline");
    assert_eq!(
        send_request(&app, Method::GET, "/api/me", &absolute, None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    context.state.pool.close().await;
}

#[tokio::test]
async fn password_reset_advances_version_and_invalidates_existing_sessions() {
    let context = TestContext::migrated().await;
    let (_admin_id, admin) = context.bootstrap().await;
    let app = routes::router(context.state.clone());
    assert_eq!(
        send_json(
            &app,
            Method::POST,
            "/api/users",
            &admin,
            json!({
                "email": "session-operator@example.com",
                "password": "operator-password",
                "role": "operator"
            }),
        )
        .await,
        StatusCode::CREATED
    );
    let (operator_id, version) = sqlx::query_as::<_, (Uuid, i64)>(
        "SELECT id, session_version FROM users WHERE email = 'session-operator@example.com'",
    )
    .fetch_one(&context.state.pool)
    .await
    .expect("load operator session version");
    let operator: BrowserCredentials = issue_session(&context.state, operator_id, version)
        .await
        .expect("issue operator session")
        .into();
    assert_eq!(
        send_request(&app, Method::GET, "/api/me", &operator, None)
            .await
            .status(),
        StatusCode::OK
    );

    assert_eq!(
        send_json(
            &app,
            Method::PUT,
            &format!("/api/users/{operator_id}"),
            &admin,
            json!({ "password": "replacement-password" }),
        )
        .await,
        StatusCode::OK
    );
    let updated_version =
        sqlx::query_scalar::<_, i64>("SELECT session_version FROM users WHERE id = ?")
            .bind(operator_id)
            .fetch_one(&context.state.pool)
            .await
            .expect("load updated operator session version");
    assert_eq!(updated_version, version + 1);
    assert_eq!(
        send_request(&app, Method::GET, "/api/me", &operator, None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    context.state.pool.close().await;
}

#[tokio::test]
async fn production_pool_configures_every_connection_and_persists_after_reopen() {
    let database = TemporaryDatabase::new();
    let database_url = database.url();
    assert!(!database.path.exists());

    let pool = sqlite::open_pool(&database_url)
        .await
        .expect("open production-configured SQLite pool");
    assert!(database.path.exists());
    sqlx::migrate!()
        .run(&pool)
        .await
        .expect("run the real migrations");

    let mut first = pool.acquire().await.expect("acquire first connection");
    let mut second = pool.acquire().await.expect("acquire second connection");
    assert_connection_configuration(&mut first).await;
    assert_connection_configuration(&mut second).await;
    assert_foreign_key_enforced(&mut first).await;
    assert_foreign_key_enforced(&mut second).await;
    drop(first);
    drop(second);

    let persisted_user = Uuid::new_v4();
    let now = Utc::now();
    sqlx::query(
        "INSERT INTO users (id, email, password_hash, role, active, created_at, updated_at) \
         VALUES (?, 'persisted@example.com', 'test-hash', 'viewer', 1, ?, ?)",
    )
    .bind(persisted_user)
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert persistent test data");
    assert_database_integrity(&pool).await;
    pool.close().await;

    let reopened = sqlite::open_pool(&database_url)
        .await
        .expect("reopen production-configured SQLite pool");
    let loaded_user =
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM users WHERE email = 'persisted@example.com'")
            .fetch_one(&reopened)
            .await
            .expect("load data after reopening");
    assert_eq!(loaded_user, persisted_user);
    let mut reopened_connection = reopened
        .acquire()
        .await
        .expect("acquire reopened connection");
    assert_connection_configuration(&mut reopened_connection).await;
    drop(reopened_connection);
    assert_database_integrity(&reopened).await;
    reopened.close().await;
}
