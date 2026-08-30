use crate::{
    auth::{
        bootstrap_admin, csrf_cookie, csrf_cookie_name, decode_media_token,
        encode_media_claims_for_test, issue_media_token, issue_session, session_cookie,
        session_cookie_name, IssuedBrowserSession,
    },
    background::{camera_path, emit_event},
    config::Config,
    crypto::SecretBox,
    login_security::LoginProtection,
    mediamtx::MediaMtxClient,
    models::CameraRecord,
    reconciliation, routes, sqlite, AppState,
};
use axum::{
    body::{to_bytes, Body},
    extract::{ConnectInfo, Path, State},
    http::{
        header::{AUTHORIZATION, COOKIE, HOST, ORIGIN, RETRY_AFTER, SET_COOKIE},
        HeaderMap, Method, Request, StatusCode,
    },
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use rusqlite::Connection as RusqliteConnection;
use serde_json::{json, Value};
use sqlx::{SqliteConnection, SqlitePool};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::broadcast, task::JoinHandle};
use tower::ServiceExt;
use uuid::Uuid;

struct TestContext {
    _database: TemporaryDatabase,
    state: AppState,
    fake_media: FakeMediaService,
    media_task: JoinHandle<()>,
    _network_guard: tokio::sync::MutexGuard<'static, ()>,
}

type OperationSideEffectState = (
    String,
    String,
    i64,
    Option<String>,
    Option<DateTime<Utc>>,
    Option<String>,
    Option<String>,
);
type CameraEnvelopeBlobs = (Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>);

#[derive(Clone, Default)]
struct FakeMediaService {
    inner: Arc<Mutex<FakeMediaState>>,
}

#[derive(Default)]
struct FakeMediaState {
    paths: HashMap<String, FakePathConfig>,
    request_calls: usize,
    mutation_calls: usize,
    fail_mutations: usize,
}

#[derive(Clone)]
struct FakePathConfig {
    source: String,
    source_on_demand: bool,
    record: bool,
}

impl FakeMediaService {
    fn router(&self) -> Router {
        Router::new()
            .route("/v3/info", get(fake_info))
            .route("/v3/config/paths/list", get(fake_config_list))
            .route("/v3/paths/list", get(fake_runtime_paths))
            .route("/v3/config/paths/get/{path}", get(fake_get_path))
            .route("/v3/config/paths/add/{path}", post(fake_upsert_path))
            .route("/v3/config/paths/patch/{path}", patch(fake_upsert_path))
            .route("/v3/config/paths/delete/{path}", delete(fake_delete_path))
            .fallback(|| async { StatusCode::OK })
            .with_state(self.clone())
    }

    fn fail_next_mutations(&self, count: usize) {
        self.inner.lock().unwrap().fail_mutations = count;
    }

    fn mutation_calls(&self) -> usize {
        self.inner.lock().unwrap().mutation_calls
    }

    fn request_calls(&self) -> usize {
        self.inner.lock().unwrap().request_calls
    }

    fn path(&self, path: &str) -> Option<FakePathConfig> {
        self.inner.lock().unwrap().paths.get(path).cloned()
    }

    fn remove_path(&self, path: &str) {
        self.inner.lock().unwrap().paths.remove(path);
    }
}

async fn fake_info(State(fake): State<FakeMediaService>) -> StatusCode {
    fake.inner.lock().unwrap().request_calls += 1;
    StatusCode::OK
}

async fn fake_config_list(State(fake): State<FakeMediaService>) -> Json<Value> {
    let mut inner = fake.inner.lock().unwrap();
    inner.request_calls += 1;
    Json(json!({
        "items": inner.paths.iter().map(|(name, config)| json!({
            "name": name,
            "source": config.source,
            "sourceOnDemand": config.source_on_demand,
            "record": config.record
        })).collect::<Vec<_>>()
    }))
}

async fn fake_runtime_paths(State(fake): State<FakeMediaService>) -> Json<Value> {
    let mut inner = fake.inner.lock().unwrap();
    inner.request_calls += 1;
    Json(json!({
        "items": inner.paths.keys().map(|name| json!({
            "name": name,
            "ready": true,
            "readers": [],
            "tracks": ["video"]
        })).collect::<Vec<_>>()
    }))
}

async fn fake_get_path(
    State(fake): State<FakeMediaService>,
    Path(path): Path<String>,
) -> StatusCode {
    let mut inner = fake.inner.lock().unwrap();
    inner.request_calls += 1;
    if inner.paths.contains_key(&path) {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn fake_upsert_path(
    State(fake): State<FakeMediaService>,
    Path(path): Path<String>,
    Json(payload): Json<Value>,
) -> Response {
    let mut inner = fake.inner.lock().unwrap();
    inner.request_calls += 1;
    inner.mutation_calls += 1;
    if inner.fail_mutations > 0 {
        inner.fail_mutations -= 1;
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "rtsp://admin:server-secret@camera.invalid/leaked-by-upstream",
        )
            .into_response();
    }
    inner.paths.insert(
        path,
        FakePathConfig {
            source: payload["source"].as_str().unwrap_or_default().to_string(),
            source_on_demand: payload["sourceOnDemand"].as_bool().unwrap_or(false),
            record: payload["record"].as_bool().unwrap_or(false),
        },
    );
    StatusCode::OK.into_response()
}

async fn fake_delete_path(
    State(fake): State<FakeMediaService>,
    Path(path): Path<String>,
) -> StatusCode {
    let mut inner = fake.inner.lock().unwrap();
    inner.request_calls += 1;
    inner.mutation_calls += 1;
    if inner.fail_mutations > 0 {
        inner.fail_mutations -= 1;
        return StatusCode::INTERNAL_SERVER_ERROR;
    }
    if inner.paths.remove(&path).is_some() {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
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
    source: Option<SocketAddr>,
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
    async fn current() -> Self {
        let network_guard = crate::NETWORK_TEST_LOCK.lock().await;
        let database = TemporaryDatabase::new();
        let database_url = database.url();
        let pool = sqlite::open_pool(&database_url)
            .await
            .expect("open temporary SQLite database");

        let media_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake media service");
        let media_address = media_listener.local_addr().expect("fake media address");
        let fake_media = FakeMediaService::default();
        let fake_service = fake_media.clone();
        let media_task = tokio::spawn(async move {
            let service = fake_service.router();
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
            runtime_directory: std::env::temp_dir(),
            bootstrap_admin_email: "admin@example.com".into(),
            bootstrap_admin_password: Some("bootstrap-password".into()),
            development_mode: true,
            session_idle_ttl: Duration::from_secs(1_800),
            session_absolute_ttl: Duration::from_secs(3_600),
            login_body_limit: 16_384,
            login_rate_capacity: 4_096,
            login_source_attempts: 30,
            login_source_window: Duration::from_secs(60),
            login_account_attempts: 10,
            login_account_window: Duration::from_secs(300),
            login_argon2_parallelism: 2,
            login_argon2_timeout: Duration::from_secs(5),
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
            static_dir: std::env::temp_dir().join("sentinel-monitor-test-web"),
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
            login: LoginProtection::new(&config),
        };

        Self {
            _database: database,
            state,
            fake_media,
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
            source: None,
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
        .extension(ConnectInfo(context.source.unwrap_or_else(|| {
            "192.0.2.10:41000".parse().expect("default test source")
        })))
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

async fn send_login(app: &Router, email: &str, password: &str, source: SocketAddr) -> Response {
    send_custom_request(
        app,
        Method::POST,
        "/api/v2/auth/login",
        BrowserRequestContext {
            host: Some("sentinel.test"),
            origin: Some("https://sentinel.test"),
            source: Some(source),
            ..Default::default()
        },
        Some(json!({ "email": email, "password": password })),
    )
    .await
}

fn media_auth_payload(token: impl Into<String>, action: &str, path: &str) -> Value {
    json!({
        "user": "",
        "password": "",
        "token": token.into(),
        "ip": "127.0.0.1",
        "action": action,
        "path": path,
        "protocol": "webrtc",
        "id": Uuid::new_v4().to_string(),
        "query": "",
        "userAgent": "sentinel-test"
    })
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

async fn assert_invalid_credentials_are_side_effect_free(
    context: &TestContext,
    observer: &RusqliteConnection,
) {
    let data_version_before: i64 = observer
        .pragma_query_value(None, "data_version", |row| row.get(0))
        .expect("read SQLite data version before rejection");
    let lease_before: (Option<String>, Option<DateTime<Utc>>, DateTime<Utc>) = sqlx::query_as(
        "SELECT lease_owner, lease_expires_at, updated_at FROM media_reconciler_leases \
         WHERE singleton = 1",
    )
    .fetch_one(&context.state.pool)
    .await
    .expect("read lease before credential rejection");
    let operations_before: Vec<OperationSideEffectState> = sqlx::query_as(
        "SELECT id, state, attempt, lease_owner, lease_expires_at, error_code, error_message \
         FROM media_operations ORDER BY id",
    )
    .fetch_all(&context.state.pool)
    .await
    .expect("read operations before credential rejection");
    let requests_before = context.fake_media.request_calls();
    let mutations_before = context.fake_media.mutation_calls();

    let error = reconciliation::reconcile_once(&context.state)
        .await
        .expect_err("invalid current credential envelope must stop reconciliation")
        .to_string();
    for sensitive in [
        "camera-user",
        "camera-password",
        "rtsp://camera.example",
        "sentinel-test-jwt-secret",
    ] {
        assert!(!error.contains(sensitive));
    }

    let data_version_after: i64 = observer
        .pragma_query_value(None, "data_version", |row| row.get(0))
        .expect("read SQLite data version after rejection");
    let lease_after: (Option<String>, Option<DateTime<Utc>>, DateTime<Utc>) = sqlx::query_as(
        "SELECT lease_owner, lease_expires_at, updated_at FROM media_reconciler_leases \
         WHERE singleton = 1",
    )
    .fetch_one(&context.state.pool)
    .await
    .expect("read lease after credential rejection");
    let operations_after: Vec<OperationSideEffectState> = sqlx::query_as(
        "SELECT id, state, attempt, lease_owner, lease_expires_at, error_code, error_message \
         FROM media_operations ORDER BY id",
    )
    .fetch_all(&context.state.pool)
    .await
    .expect("read operations after credential rejection");
    assert_eq!(data_version_after, data_version_before);
    assert_eq!(lease_after, lease_before);
    assert_eq!(operations_after, operations_before);
    assert_eq!(context.fake_media.request_calls(), requests_before);
    assert_eq!(context.fake_media.mutation_calls(), mutations_before);
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
async fn current_database_accepts_all_required_creation_timestamps() {
    let context = TestContext::current().await;
    let (admin_id, token) = context.bootstrap().await;
    let app = routes::router(context.state.clone());

    let (created_at, updated_at) = required_times(&context.state.pool, "users", admin_id).await;
    assert_utc_timestamp(&created_at);
    assert_utc_timestamp(updated_at.as_deref().expect("administrator updated_at"));

    assert_eq!(
        send_json(
            &app,
            Method::POST,
            "/api/v2/users",
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
            "/api/v2/cameras",
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
async fn current_database_updates_cameras_filters_and_acknowledges_events() {
    let context = TestContext::current().await;
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

    let response = send_request(&app, Method::GET, "/api/v2/me", &token, None).await;
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
                "/api/v2/cameras",
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
        &format!("/api/v2/cameras/{target_id}"),
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
    assert_eq!(response["media_synced"], json!(false));
    assert_eq!(response["operation_state"], json!("pending"));
    assert!(response["operation_id"]
        .as_str()
        .is_some_and(|id| !id.is_empty()));
    assert_eq!(response["camera"]["id"], json!(target_id));
    assert_eq!(response["camera"]["status"], json!("disabled"));

    let camera = sqlx::query_as::<_, CameraRecord>(
        "SELECT id, name, location, main_stream_url_enc, sub_stream_url_enc, onvif_url, username_enc, password_enc, enabled, record_enabled, status, last_seen_at, created_at, updated_at FROM cameras WHERE id = ?",
    )
    .bind(target_id)
    .fetch_one(&context.state.pool)
    .await
    .expect("load updated camera");
    assert_eq!(camera.name, "Updated Camera");
    assert_eq!(camera.location, "Garage");
    let credentials = camera
        .decrypt_credentials(&context.state.secrets)
        .expect("decrypt updated camera credentials");
    assert_eq!(
        credentials.main_stream_url,
        "rtsps://camera.example/updated-main"
    );
    assert_eq!(
        credentials.sub_stream_url.as_deref(),
        Some("rtsp://camera.example/updated-sub")
    );
    assert_eq!(
        camera.onvif_url.as_deref(),
        Some("https://camera.example/onvif/device_service")
    );
    assert_eq!(credentials.username.as_deref(), Some("camera-user"));
    assert_eq!(credentials.password.as_deref(), Some("camera-password"));
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
        &format!("/api/v2/events/{}/ack", acknowledged.id),
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
        &format!("/api/v2/events?camera_id={target_id}&unacknowledged=true&limit=10"),
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
        &format!("/api/v2/events?camera_id={target_id}&limit=10"),
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
async fn invalid_camera_envelopes_fail_before_database_or_media_side_effects() {
    let context = TestContext::current().await;
    let (_, admin) = context.bootstrap().await;
    let app = routes::router(context.state.clone());
    let mut camera_ids = Vec::new();
    for (name, suffix) in [("Envelope One", "one"), ("Envelope Two", "two")] {
        let response = send_request(
            &app,
            Method::POST,
            "/api/v2/cameras",
            &admin,
            Some(json!({
                "name": name,
                "main_stream_url": format!("rtsp://camera.example/{suffix}/main"),
                "sub_stream_url": format!("rtsp://camera.example/{suffix}/sub"),
                "username": format!("camera-user-{suffix}"),
                "password": format!("camera-password-{suffix}")
            })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = response_json(response).await;
        assert_eq!(body["camera"]["username"], format!("camera-user-{suffix}"));
        let serialized = body.to_string();
        assert!(!serialized.contains(&format!("camera-password-{suffix}")));
        assert!(!serialized.contains(&format!("rtsp://camera.example/{suffix}")));
        camera_ids.push(
            body["camera"]["id"]
                .as_str()
                .expect("camera ID")
                .parse::<Uuid>()
                .expect("camera UUID"),
        );
    }
    let first = camera_ids[0];
    let second = camera_ids[1];
    let first_blobs: CameraEnvelopeBlobs = sqlx::query_as(
        "SELECT main_stream_url_enc, sub_stream_url_enc, username_enc, password_enc \
             FROM cameras WHERE id = ?",
    )
    .bind(first)
    .fetch_one(&context.state.pool)
    .await
    .expect("load first camera envelopes");
    let second_main: Vec<u8> =
        sqlx::query_scalar("SELECT main_stream_url_enc FROM cameras WHERE id = ?")
            .bind(second)
            .fetch_one(&context.state.pool)
            .await
            .expect("load second main envelope");

    let envelope: Value = serde_json::from_slice(&first_blobs.0).expect("current envelope JSON");
    assert_eq!(envelope["product"], "sentinel-monitor");
    assert_eq!(envelope["application_version"], "0.2.0");
    assert_eq!(envelope["envelope_revision"], 1);
    assert_eq!(envelope["key_id"], "sentinel-credentials-0.2.0-key-1");
    let keys = envelope
        .as_object()
        .expect("credential envelope object")
        .keys()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        keys,
        std::collections::BTreeSet::from([
            "application_version",
            "ciphertext",
            "envelope_revision",
            "key_id",
            "nonce",
            "product",
        ])
    );
    for secret in [
        "camera-user-one",
        "camera-password-one",
        "rtsp://camera.example/one",
    ] {
        assert!(!String::from_utf8_lossy(&first_blobs.0).contains(secret));
        assert!(
            !String::from_utf8_lossy(first_blobs.2.as_deref().expect("username envelope"))
                .contains(secret)
        );
        assert!(
            !String::from_utf8_lossy(first_blobs.3.as_deref().expect("password envelope"))
                .contains(secret)
        );
    }

    let observer = RusqliteConnection::open(&context._database.path)
        .expect("open independent SQLite observer");
    observer
        .busy_timeout(Duration::from_secs(5))
        .expect("configure SQLite observer");

    let mut invalid_envelopes = vec![vec![0x11; 48]];
    for (field, value) in [
        ("product", json!("another-product")),
        ("application_version", json!("0.1.0")),
        ("envelope_revision", json!(2)),
        ("key_id", json!("previous-key")),
        ("unknown", json!(true)),
    ] {
        let mut invalid = envelope.clone();
        invalid
            .as_object_mut()
            .expect("envelope object")
            .insert(field.into(), value);
        invalid_envelopes.push(serde_json::to_vec(&invalid).expect("serialize invalid envelope"));
    }
    let mut tampered = envelope.clone();
    let ciphertext = tampered["ciphertext"]
        .as_str()
        .expect("ciphertext")
        .as_bytes()
        .to_vec();
    let mut changed = ciphertext;
    changed[0] = if changed[0] == b'A' { b'B' } else { b'A' };
    tampered["ciphertext"] = String::from_utf8(changed)
        .expect("base64 remains UTF-8")
        .into();
    invalid_envelopes.push(serde_json::to_vec(&tampered).expect("serialize tampered envelope"));

    for invalid in invalid_envelopes {
        sqlx::query("UPDATE cameras SET main_stream_url_enc = ? WHERE id = ?")
            .bind(invalid)
            .bind(first)
            .execute(&context.state.pool)
            .await
            .expect("inject invalid envelope");
        assert_invalid_credentials_are_side_effect_free(&context, &observer).await;
    }
    sqlx::query("UPDATE cameras SET main_stream_url_enc = ? WHERE id = ?")
        .bind(&first_blobs.0)
        .bind(first)
        .execute(&context.state.pool)
        .await
        .expect("restore first main envelope");

    sqlx::query(
        "UPDATE cameras SET username_enc = password_enc, password_enc = username_enc WHERE id = ?",
    )
    .bind(first)
    .execute(&context.state.pool)
    .await
    .expect("swap username and password envelopes");
    assert_invalid_credentials_are_side_effect_free(&context, &observer).await;
    let camera_before: (String, String, DateTime<Utc>) =
        sqlx::query_as("SELECT name, status, updated_at FROM cameras WHERE id = ?")
            .bind(first)
            .fetch_one(&context.state.pool)
            .await
            .expect("snapshot camera before rejected update");
    let generation_before: i64 =
        sqlx::query_scalar("SELECT generation FROM media_desired_states WHERE camera_id = ?")
            .bind(first)
            .fetch_one(&context.state.pool)
            .await
            .expect("snapshot desired generation before rejected update");
    let operation_count_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM media_operations")
        .fetch_one(&context.state.pool)
        .await
        .expect("snapshot operation count before rejected update");
    let audit_count_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_logs")
        .fetch_one(&context.state.pool)
        .await
        .expect("snapshot audit count before rejected update");
    let media_requests_before = context.fake_media.request_calls();
    let response = send_request(
        &app,
        Method::PUT,
        &format!("/api/v2/cameras/{first}"),
        &admin,
        Some(json!({ "name": "must-not-be-written" })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let error_body = response_json(response).await.to_string();
    for sensitive in [
        "camera-user-one",
        "camera-password-one",
        "rtsp://camera.example/one",
    ] {
        assert!(!error_body.contains(sensitive));
    }
    let camera_after: (String, String, DateTime<Utc>) =
        sqlx::query_as("SELECT name, status, updated_at FROM cameras WHERE id = ?")
            .bind(first)
            .fetch_one(&context.state.pool)
            .await
            .expect("verify camera after rejected update");
    let generation_after: i64 =
        sqlx::query_scalar("SELECT generation FROM media_desired_states WHERE camera_id = ?")
            .bind(first)
            .fetch_one(&context.state.pool)
            .await
            .expect("verify desired generation after rejected update");
    let operation_count_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM media_operations")
        .fetch_one(&context.state.pool)
        .await
        .expect("verify operation count after rejected update");
    let audit_count_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_logs")
        .fetch_one(&context.state.pool)
        .await
        .expect("verify audit count after rejected update");
    assert_eq!(camera_after, camera_before);
    assert_eq!(generation_after, generation_before);
    assert_eq!(operation_count_after, operation_count_before);
    assert_eq!(audit_count_after, audit_count_before);
    assert_eq!(context.fake_media.request_calls(), media_requests_before);
    sqlx::query("UPDATE cameras SET username_enc = ?, password_enc = ? WHERE id = ?")
        .bind(first_blobs.2.as_deref())
        .bind(first_blobs.3.as_deref())
        .bind(first)
        .execute(&context.state.pool)
        .await
        .expect("restore username and password envelopes");

    let mut transaction = context.state.pool.begin().await.expect("begin camera swap");
    sqlx::query("UPDATE cameras SET main_stream_url_enc = ? WHERE id = ?")
        .bind(&second_main)
        .bind(first)
        .execute(&mut *transaction)
        .await
        .expect("install second camera envelope on first camera");
    sqlx::query("UPDATE cameras SET main_stream_url_enc = ? WHERE id = ?")
        .bind(&first_blobs.0)
        .bind(second)
        .execute(&mut *transaction)
        .await
        .expect("install first camera envelope on second camera");
    transaction.commit().await.expect("commit camera swap");
    assert_invalid_credentials_are_side_effect_free(&context, &observer).await;

    sqlx::query("UPDATE cameras SET main_stream_url_enc = ? WHERE id = ?")
        .bind(&first_blobs.0)
        .bind(first)
        .execute(&context.state.pool)
        .await
        .expect("restore first camera envelope");
    sqlx::query("UPDATE cameras SET main_stream_url_enc = ? WHERE id = ?")
        .bind(&second_main)
        .bind(second)
        .execute(&context.state.pool)
        .await
        .expect("restore second camera envelope");
    for camera_id in camera_ids {
        let camera = sqlx::query_as::<_, CameraRecord>(&format!(
            "{select} WHERE id = ?",
            select = "SELECT id, name, location, main_stream_url_enc, sub_stream_url_enc, onvif_url, username_enc, password_enc, enabled, record_enabled, status, last_seen_at, created_at, updated_at FROM cameras"
        ))
        .bind(camera_id)
        .fetch_one(&context.state.pool)
        .await
        .expect("reload restored camera");
        camera
            .decrypt_credentials(&context.state.secrets)
            .expect("restored envelopes authenticate");
    }
    drop(observer);
    context.state.pool.close().await;
}

#[tokio::test]
async fn corrupt_global_lease_fails_fast_without_side_effects_and_recovers() {
    let context = TestContext::current().await;
    let (_, admin) = context.bootstrap().await;
    let app = routes::router(context.state.clone());
    let response = send_request(
        &app,
        Method::POST,
        "/api/v2/cameras",
        &admin,
        Some(json!({
            "name": "Lease Contract Camera",
            "main_stream_url": "rtsp://camera.example/lease-contract",
            "enabled": true,
            "record_enabled": false
        })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let mut corrupter =
        RusqliteConnection::open(&context._database.path).expect("open raw global lease corrupter");
    corrupter
        .busy_timeout(Duration::from_secs(5))
        .expect("configure raw global lease corrupter");
    corrupter
        .execute_batch(
            "PRAGMA wal_autocheckpoint=0;
             PRAGMA ignore_check_constraints=ON;",
        )
        .expect("model an out-of-protocol SQLite writer");
    let original: (Option<String>, Option<String>, String) = corrupter
        .query_row(
            "SELECT lease_owner, lease_expires_at, updated_at
             FROM media_reconciler_leases WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("snapshot the current global lease row");
    assert!(original.0.is_none());
    assert!(original.1.is_none());

    let observer = RusqliteConnection::open(&context._database.path)
        .expect("open independent global lease observer");
    observer
        .busy_timeout(Duration::from_secs(5))
        .expect("configure independent global lease observer");
    let corruptions = [
        (
            "missing singleton row",
            "DELETE FROM media_reconciler_leases;",
        ),
        (
            "extra singleton row",
            "INSERT INTO media_reconciler_leases (singleton, updated_at)
             VALUES (2, '1970-01-01T00:00:00+00:00');",
        ),
        (
            "owner without expiry",
            "UPDATE media_reconciler_leases
             SET lease_owner = '00000000-0000-4000-8000-000000000001',
                 lease_expires_at = NULL WHERE singleton = 1;",
        ),
        (
            "noncanonical lease timestamp",
            "UPDATE media_reconciler_leases
             SET lease_owner = '00000000-0000-4000-8000-000000000001',
                 lease_expires_at = '2030-01-01 00:01:00',
                 updated_at = '2030-01-01 00:00:00' WHERE singleton = 1;",
        ),
    ];

    for (name, mutation) in corruptions {
        corrupter
            .execute_batch(mutation)
            .unwrap_or_else(|error| panic!("inject {name}: {error}"));
        let wal = PathBuf::from(format!("{}-wal", context._database.path.display()));
        assert!(wal.exists(), "{name} must be committed through real WAL");

        let data_version_before: i64 = observer
            .pragma_query_value(None, "data_version", |row| row.get(0))
            .expect("read data version before corrupt lease rejection");
        let operations_before: Vec<OperationSideEffectState> = sqlx::query_as(
            "SELECT id, state, attempt, lease_owner, lease_expires_at, error_code, error_message
             FROM media_operations ORDER BY id",
        )
        .fetch_all(&context.state.pool)
        .await
        .expect("snapshot operations before corrupt lease rejection");
        let desired_before: Vec<(Uuid, i64, DateTime<Utc>)> = sqlx::query_as(
            "SELECT camera_id, generation, updated_at FROM media_desired_states ORDER BY camera_id",
        )
        .fetch_all(&context.state.pool)
        .await
        .expect("snapshot desired state before corrupt lease rejection");
        let cameras_before: Vec<(Uuid, String, DateTime<Utc>)> =
            sqlx::query_as("SELECT id, status, updated_at FROM cameras ORDER BY id")
                .fetch_all(&context.state.pool)
                .await
                .expect("snapshot cameras before corrupt lease rejection");
        let audit_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_logs")
            .fetch_one(&context.state.pool)
            .await
            .expect("snapshot audit rows before corrupt lease rejection");
        let media_requests_before = context.fake_media.request_calls();
        let media_mutations_before = context.fake_media.mutation_calls();

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            reconciliation::reconcile_available(&context.state),
        )
        .await
        .unwrap_or_else(|_| panic!("{name} caused a reconciler block or busy loop"));
        let error = result
            .expect_err("corrupt global lease must fail closed")
            .to_string();
        assert!(error.contains("global lease"));

        let data_version_after: i64 = observer
            .pragma_query_value(None, "data_version", |row| row.get(0))
            .expect("read data version after corrupt lease rejection");
        let operations_after: Vec<OperationSideEffectState> = sqlx::query_as(
            "SELECT id, state, attempt, lease_owner, lease_expires_at, error_code, error_message
             FROM media_operations ORDER BY id",
        )
        .fetch_all(&context.state.pool)
        .await
        .expect("verify operations after corrupt lease rejection");
        let desired_after: Vec<(Uuid, i64, DateTime<Utc>)> = sqlx::query_as(
            "SELECT camera_id, generation, updated_at FROM media_desired_states ORDER BY camera_id",
        )
        .fetch_all(&context.state.pool)
        .await
        .expect("verify desired state after corrupt lease rejection");
        let cameras_after: Vec<(Uuid, String, DateTime<Utc>)> =
            sqlx::query_as("SELECT id, status, updated_at FROM cameras ORDER BY id")
                .fetch_all(&context.state.pool)
                .await
                .expect("verify cameras after corrupt lease rejection");
        let audit_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_logs")
            .fetch_one(&context.state.pool)
            .await
            .expect("verify audit rows after corrupt lease rejection");
        assert_eq!(data_version_after, data_version_before, "{name}");
        assert_eq!(operations_after, operations_before, "{name}");
        assert_eq!(desired_after, desired_before, "{name}");
        assert_eq!(cameras_after, cameras_before, "{name}");
        assert_eq!(audit_after, audit_before, "{name}");
        assert_eq!(context.fake_media.request_calls(), media_requests_before);
        assert_eq!(context.fake_media.mutation_calls(), media_mutations_before);

        let restore = corrupter
            .transaction()
            .expect("begin raw global lease restore transaction");
        restore
            .execute("DELETE FROM media_reconciler_leases", [])
            .expect("remove corrupt global lease rows");
        restore
            .execute(
                "INSERT INTO media_reconciler_leases
                     (singleton, lease_owner, lease_expires_at, updated_at)
                 VALUES (1, ?1, ?2, ?3)",
                rusqlite::params![original.0.as_deref(), original.1.as_deref(), &original.2],
            )
            .expect("restore exact current global lease row");
        restore.commit().expect("commit global lease row restore");
    }

    assert!(reconciliation::reconcile_once(&context.state)
        .await
        .expect("reconciler resumes when exact current state returns"));
    assert!(context.fake_media.mutation_calls() > 0);
    drop(observer);
    drop(corrupter);
    context.state.pool.close().await;
}

#[tokio::test]
async fn browser_session_persists_across_reopen_and_machine_tokens_remain_separate() {
    let mut context = TestContext::current().await;
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
        "/api/v2/auth/login",
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
        "/api/v2/auth/login",
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
        "/api/v2/auth/login",
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
        .uri("/api/v2/me")
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
        "/internal/v2/media/auth",
        BrowserRequestContext::default(),
        Some(media_auth_payload(machine_token, "read", "camera/main")),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    assert_eq!(
        send_request(&app, Method::GET, "/api/v2/me", &credentials, None)
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
        send_request(
            &restarted_app,
            Method::GET,
            "/api/v2/me",
            &credentials,
            None,
        )
        .await
        .status(),
        StatusCode::OK
    );
    context.state.pool.close().await;
}

#[tokio::test]
async fn protocol_v1_routes_json_and_tokens_are_rejected_without_side_effects() {
    let context = TestContext::current().await;
    bootstrap_admin(&context.state)
        .await
        .expect("bootstrap administrator");
    let admin_id = sqlx::query_scalar::<_, Uuid>("SELECT id FROM users WHERE role = 'admin'")
        .fetch_one(&context.state.pool)
        .await
        .expect("load administrator id");
    let app = routes::router(context.state.clone());
    let before: (i64, i64, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM browser_sessions), \
                (SELECT COUNT(*) FROM audit_logs), \
                (SELECT last_login_at FROM users WHERE id = ?)",
    )
    .bind(admin_id)
    .fetch_one(&context.state.pool)
    .await
    .expect("capture protocol side effects");

    let old_login = send_custom_request(
        &app,
        Method::POST,
        "/api/auth/login",
        BrowserRequestContext {
            host: Some("sentinel.test"),
            origin: Some("https://sentinel.test"),
            ..Default::default()
        },
        Some(json!({
            "email": "admin@example.com",
            "password": "bootstrap-password"
        })),
    )
    .await;
    assert!(!old_login.status().is_success());
    let old_me = send_custom_request(
        &app,
        Method::GET,
        "/api/me",
        BrowserRequestContext::default(),
        None,
    )
    .await;
    assert!(!old_me.status().is_success());

    for malformed_login in [
        json!({
            "email": "admin@example.com",
            "password": "bootstrap-password",
            "remember": true
        }),
        json!({ "email": "admin@example.com" }),
    ] {
        let response = send_custom_request(
            &app,
            Method::POST,
            "/api/v2/auth/login",
            BrowserRequestContext {
                host: Some("sentinel.test"),
                origin: Some("https://sentinel.test"),
                ..Default::default()
            },
            Some(malformed_login),
        )
        .await;
        assert!(response.status().is_client_error());
    }

    let camera_id = Uuid::new_v4();
    let (current_token, _) = issue_media_token(
        admin_id,
        camera_id,
        "camera/main".into(),
        vec!["read".into()],
        &context.state.config,
    )
    .expect("issue current media token");
    let current_claims = decode_media_token(&current_token, &context.state.config)
        .expect("decode current media token");
    assert_eq!(current_claims.sub, admin_id);
    assert!(!current_claims.jti.is_nil());
    assert_eq!(current_claims.nbf, current_claims.iat);
    assert_eq!(
        current_claims.exp - current_claims.iat,
        context.state.config.media_token_ttl.as_secs()
    );

    let current_payload = media_auth_payload(current_token.clone(), "read", "camera/main");
    let response = send_custom_request(
        &app,
        Method::POST,
        "/internal/v2/media/auth",
        BrowserRequestContext::default(),
        Some(current_payload.clone()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let response = send_custom_request(
        &app,
        Method::POST,
        "/internal/media/auth",
        BrowserRequestContext::default(),
        Some(current_payload.clone()),
    )
    .await;
    assert!(!response.status().is_success());

    let now = Utc::now().timestamp() as u64;
    let legacy_token = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &json!({
            "sub": admin_id.to_string(),
            "camera_id": camera_id,
            "path": "camera/main",
            "actions": ["read"],
            "kind": "media",
            "iat": now,
            "exp": now + 300
        }),
        &jsonwebtoken::EncodingKey::from_secret(&context.state.config.jwt_secret),
    )
    .expect("encode pre-0.2 media token");
    let response = send_custom_request(
        &app,
        Method::POST,
        "/internal/v2/media/auth",
        BrowserRequestContext::default(),
        Some(media_auth_payload(legacy_token, "read", "camera/main")),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let mut invalid_id_claims = current_claims.clone();
    invalid_id_claims.jti = Uuid::nil();
    let mut invalid_time_claims = current_claims.clone();
    invalid_time_claims.exp += 1;
    let mut unknown_claims = serde_json::to_value(&current_claims).expect("serialize claims");
    unknown_claims
        .as_object_mut()
        .expect("claims object")
        .insert("legacy".into(), json!(true));
    let mut missing_claims = serde_json::to_value(&current_claims).expect("serialize claims");
    missing_claims
        .as_object_mut()
        .expect("claims object")
        .remove("jti");
    for invalid_claims in [
        serde_json::to_value(invalid_id_claims).expect("serialize invalid ID claims"),
        serde_json::to_value(invalid_time_claims).expect("serialize invalid time claims"),
        unknown_claims,
        missing_claims,
    ] {
        let token = encode_media_claims_for_test(&invalid_claims, &context.state.config);
        let response = send_custom_request(
            &app,
            Method::POST,
            "/internal/v2/media/auth",
            BrowserRequestContext::default(),
            Some(media_auth_payload(token, "read", "camera/main")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    let mut unknown_media_json = current_payload.clone();
    unknown_media_json
        .as_object_mut()
        .expect("media payload object")
        .insert("legacy".into(), json!(true));
    let mut missing_media_json = current_payload;
    missing_media_json
        .as_object_mut()
        .expect("media payload object")
        .remove("userAgent");
    for malformed_media_json in [
        json!({
            "token": current_token,
            "action": "read",
            "path": "camera/main"
        }),
        unknown_media_json,
        missing_media_json,
    ] {
        let response = send_custom_request(
            &app,
            Method::POST,
            "/internal/v2/media/auth",
            BrowserRequestContext::default(),
            Some(malformed_media_json),
        )
        .await;
        assert!(response.status().is_client_error());
    }

    let after: (i64, i64, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM browser_sessions), \
                (SELECT COUNT(*) FROM audit_logs), \
                (SELECT last_login_at FROM users WHERE id = ?)",
    )
    .bind(admin_id)
    .fetch_one(&context.state.pool)
    .await
    .expect("verify protocol side effects");
    assert_eq!(after, before);
    context.state.pool.close().await;
}

#[tokio::test]
async fn browser_session_enforces_bound_csrf_origin_revocation_and_both_expiries() {
    let context = TestContext::current().await;
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
        "/api/v2/auth/logout",
        BrowserRequestContext {
            credentials: Some(&first),
            csrf_token: Some(&second.csrf_token),
            host: Some("sentinel.test"),
            origin: Some("https://sentinel.test"),
            source: None,
        },
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = send_custom_request(
        &app,
        Method::POST,
        "/api/v2/auth/logout",
        BrowserRequestContext {
            credentials: Some(&first),
            csrf_token: Some(&first.csrf_token),
            host: Some("sentinel.test"),
            origin: Some("https://attacker.test"),
            source: None,
        },
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = send_custom_request(
        &app,
        Method::POST,
        "/api/v2/auth/logout",
        BrowserRequestContext {
            credentials: Some(&first),
            csrf_token: Some(&first.csrf_token),
            host: Some("sentinel.test"),
            origin: None,
            source: None,
        },
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let response = send_request(&app, Method::POST, "/api/v2/auth/logout", &first, None).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let revoked: bool =
        sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM browser_sessions WHERE id = ?")
            .bind(first.session_id.expect("first session id"))
            .fetch_one(&context.state.pool)
            .await
            .expect("load revoked session state");
    assert!(revoked);
    assert_eq!(
        send_request(&app, Method::GET, "/api/v2/me", &first, None)
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
        send_request(&app, Method::GET, "/api/v2/me", &second, None)
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
        send_request(&app, Method::GET, "/api/v2/me", &absolute, None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    context.state.pool.close().await;
}

#[tokio::test]
async fn password_reset_advances_version_and_invalidates_existing_sessions() {
    let context = TestContext::current().await;
    let (_admin_id, admin) = context.bootstrap().await;
    let app = routes::router(context.state.clone());
    assert_eq!(
        send_json(
            &app,
            Method::POST,
            "/api/v2/users",
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
        send_request(&app, Method::GET, "/api/v2/me", &operator, None)
            .await
            .status(),
        StatusCode::OK
    );

    assert_eq!(
        send_json(
            &app,
            Method::PUT,
            &format!("/api/v2/users/{operator_id}"),
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
        send_request(&app, Method::GET, "/api/v2/me", &operator, None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    context.state.pool.close().await;
}

#[tokio::test]
async fn login_budget_limits_body_and_both_bounded_rate_dimensions() {
    let mut context = TestContext::current().await;
    bootstrap_admin(&context.state)
        .await
        .expect("bootstrap administrator");
    context.state.login = LoginProtection::for_test(2, 10, 2, 2, Duration::from_secs(5));
    let app = routes::router(context.state.clone());

    let oversized = send_login(
        &app,
        &"x".repeat(context.state.config.login_body_limit + 1),
        "irrelevant-password",
        "192.0.2.1:41000".parse().unwrap(),
    )
    .await;
    assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);

    for source in ["192.0.2.11:41000", "192.0.2.12:41000"] {
        let response = send_login(
            &app,
            "unknown@example.com",
            "wrong-password",
            source.parse().unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let response = send_login(
        &app,
        "UNKNOWN@example.com",
        "wrong-password",
        "192.0.2.13:41000".parse().unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response.headers().contains_key(RETRY_AFTER));

    drop(app);
    context.state.login = LoginProtection::for_test(2, 2, 10, 2, Duration::from_secs(5));
    let app = routes::router(context.state.clone());
    let source = "198.51.100.20:42000".parse().unwrap();
    for account in ["one@example.com", "two@example.com"] {
        let response = send_login(&app, account, "wrong-password", source).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let response = send_login(&app, "three@example.com", "wrong-password", source).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .expect("429 must include Retry-After")
        .to_str()
        .expect("Retry-After header value")
        .parse::<u64>()
        .expect("Retry-After seconds");
    assert!(retry_after >= 1);
    context.state.pool.close().await;
}

#[tokio::test]
async fn login_argon2_global_gate_times_out_with_retry_after() {
    let mut context = TestContext::current().await;
    bootstrap_admin(&context.state)
        .await
        .expect("bootstrap administrator");
    context.state.login = LoginProtection::for_test(8, 100, 100, 0, Duration::from_millis(20));
    let app = routes::router(context.state.clone());

    let response = send_login(
        &app,
        "admin@example.com",
        "bootstrap-password",
        "203.0.113.10:43000".parse().unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response
            .headers()
            .get(RETRY_AFTER)
            .expect("timeout must include Retry-After"),
        "1"
    );
    let session_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM browser_sessions")
        .fetch_one(&context.state.pool)
        .await
        .expect("count sessions after timed out verifier");
    assert_eq!(session_count, 0);
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

#[tokio::test]
async fn media_write_persists_desired_operation_before_side_effect_and_tracks_actual() {
    let context = TestContext::current().await;
    let (_admin_id, admin) = context.bootstrap().await;
    let app = routes::router(context.state.clone());

    let response = send_request(
        &app,
        Method::POST,
        "/api/v2/cameras",
        &admin,
        Some(json!({
            "name": "Queued Camera",
            "main_stream_url": "rtsp://camera.example/live",
            "username": "camera-user",
            "password": "camera-secret",
            "enabled": true,
            "record_enabled": true
        })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let response = response_json(response).await;
    assert_eq!(response["media_synced"], false);
    assert_eq!(response["operation_state"], "pending");
    let operation_id = response["operation_id"]
        .as_str()
        .expect("operation id")
        .to_string();
    let camera_id: Uuid = serde_json::from_value(response["camera"]["id"].clone()).unwrap();
    assert_eq!(context.fake_media.mutation_calls(), 0);

    let desired: (i64, bool, String) = sqlx::query_as(
        "SELECT generation, desired_present, main_path FROM media_desired_states WHERE camera_id = ?",
    )
    .bind(camera_id)
    .fetch_one(&context.state.pool)
    .await
    .expect("load desired state before media side effect");
    assert_eq!(desired.0, 1);
    assert!(desired.1);
    assert_eq!(desired.2, format!("cam_{}_main", camera_id.simple()));

    assert!(reconciliation::reconcile_once(&context.state)
        .await
        .expect("run queued reconciliation"));
    let operation = reconciliation::get_operation(&context.state.pool, &operation_id)
        .await
        .expect("load completed operation");
    assert_eq!(operation.state, "succeeded");
    assert_eq!(operation.attempt, 1);
    let path = camera_path(camera_id, "main");
    let fake_path = context
        .fake_media
        .path(&path)
        .expect("configured media path");
    assert_eq!(
        fake_path.source,
        "rtsp://camera-user:camera-secret@camera.example/live"
    );
    assert!(fake_path.record);
    assert!(!fake_path.source_on_demand);

    let actual: (bool, bool, Option<Vec<u8>>, Option<i64>, Option<String>) = sqlx::query_as(
        "SELECT present, record_configured, source_digest, applied_generation, last_operation_id \
         FROM media_actual_paths WHERE path_name = ?",
    )
    .bind(&path)
    .fetch_one(&context.state.pool)
    .await
    .expect("load persisted actual state");
    assert!(actual.0);
    assert!(actual.1);
    assert_eq!(actual.2.expect("source digest").len(), 32);
    assert_eq!(actual.3, Some(1));
    assert_eq!(actual.4.as_deref(), Some(operation_id.as_str()));

    let operation_response = send_request(
        &app,
        Method::GET,
        &format!("/api/v2/media/operations/{operation_id}"),
        &admin,
        None,
    )
    .await;
    assert_eq!(operation_response.status(), StatusCode::OK);
    let serialized = response_json(operation_response).await.to_string();
    assert!(!serialized.contains("camera-secret"));
    assert!(!serialized.contains("camera.example"));

    let delete_response = send_request(
        &app,
        Method::DELETE,
        &format!("/api/v2/cameras/{camera_id}"),
        &admin,
        None,
    )
    .await;
    assert_eq!(delete_response.status(), StatusCode::ACCEPTED);
    let delete_operation = response_json(delete_response).await;
    assert_eq!(delete_operation["state"], "pending");
    let delete_operation_id = delete_operation["id"].as_str().unwrap().to_string();
    assert!(context.fake_media.path(&path).is_some());
    let cameras =
        response_json(send_request(&app, Method::GET, "/api/v2/cameras", &admin, None).await).await;
    assert!(cameras.as_array().unwrap().is_empty());
    assert!(reconciliation::reconcile_once(&context.state)
        .await
        .expect("apply queued media cleanup"));
    assert!(context.fake_media.path(&path).is_none());
    assert_eq!(
        reconciliation::get_operation(&context.state.pool, &delete_operation_id)
            .await
            .expect("load completed delete operation")
            .state,
        "succeeded"
    );
    let desired_present: bool =
        sqlx::query_scalar("SELECT desired_present FROM media_desired_states WHERE camera_id = ?")
            .bind(camera_id)
            .fetch_one(&context.state.pool)
            .await
            .expect("load deleted camera desired state");
    assert!(!desired_present);
    context.state.pool.close().await;
}

#[tokio::test]
async fn media_failure_is_sanitized_and_retries_to_success() {
    let context = TestContext::current().await;
    let (_admin_id, admin) = context.bootstrap().await;
    let app = routes::router(context.state.clone());
    let response = send_request(
        &app,
        Method::POST,
        "/api/v2/cameras",
        &admin,
        Some(json!({
            "name": "Retry Camera",
            "main_stream_url": "rtsp://camera.invalid/live",
            "username": "admin",
            "password": "database-secret",
            "enabled": true,
            "record_enabled": false
        })),
    )
    .await;
    let payload = response_json(response).await;
    let operation_id = payload["operation_id"].as_str().unwrap().to_string();
    context.fake_media.fail_next_mutations(1);

    assert!(reconciliation::reconcile_once(&context.state)
        .await
        .expect("record known media failure"));
    let failed = reconciliation::get_operation(&context.state.pool, &operation_id)
        .await
        .expect("load failed operation");
    assert_eq!(failed.state, "failed");
    assert_eq!(failed.error_code.as_deref(), Some("media_request_failed"));
    assert!(failed.retry_at.is_some());
    let persisted_error = failed.error_message.unwrap_or_default();
    for forbidden in [
        "database-secret",
        "server-secret",
        "camera.invalid",
        "rtsp://",
    ] {
        assert!(!persisted_error.contains(forbidden), "leaked {forbidden}");
    }

    sqlx::query("UPDATE media_operations SET retry_at = ? WHERE id = ?")
        .bind(Utc::now() - chrono::Duration::seconds(1))
        .bind(&operation_id)
        .execute(&context.state.pool)
        .await
        .expect("make retry immediately due");
    assert!(reconciliation::reconcile_once(&context.state)
        .await
        .expect("retry media reconciliation"));
    let succeeded = reconciliation::get_operation(&context.state.pool, &operation_id)
        .await
        .expect("load retried operation");
    assert_eq!(succeeded.state, "succeeded");
    assert_eq!(succeeded.attempt, 2);
    assert!(succeeded.error_message.is_none());
    context.state.pool.close().await;
}

#[tokio::test]
async fn media_startup_preserves_active_leases_and_recovers_only_expired_work() {
    let context = TestContext::current().await;
    let (_admin_id, admin) = context.bootstrap().await;
    let app = routes::router(context.state.clone());
    let response = send_request(
        &app,
        Method::POST,
        "/api/v2/cameras",
        &admin,
        Some(json!({
            "name": "Restart Camera",
            "main_stream_url": "rtsp://camera.example/restart",
            "enabled": true,
            "record_enabled": false
        })),
    )
    .await;
    let payload = response_json(response).await;
    let operation_id = payload["operation_id"].as_str().unwrap().to_string();
    let now = Utc::now();
    let active_until = now + chrono::Duration::minutes(1);
    let healthy_owner = Uuid::new_v4().to_string();
    sqlx::query(
        "UPDATE media_operations SET state = 'running', attempt = 1, started_at = ?, \
         lease_owner = ?, lease_expires_at = ? WHERE id = ?",
    )
    .bind(now)
    .bind(&healthy_owner)
    .bind(active_until)
    .bind(&operation_id)
    .execute(&context.state.pool)
    .await
    .expect("simulate active worker");
    sqlx::query(
        "UPDATE media_reconciler_leases SET lease_owner = ?, \
         lease_expires_at = ?, updated_at = ? WHERE singleton = 1",
    )
    .bind(&healthy_owner)
    .bind(active_until)
    .bind(now)
    .execute(&context.state.pool)
    .await
    .expect("record active global lease");

    assert_eq!(
        reconciliation::recover_interrupted_operations(&context.state.pool)
            .await
            .expect("preserve active operation"),
        0
    );
    let active: (String, Option<String>) =
        sqlx::query_as("SELECT state, lease_owner FROM media_operations WHERE id = ?")
            .bind(&operation_id)
            .fetch_one(&context.state.pool)
            .await
            .expect("load active operation");
    assert_eq!(active, ("running".into(), Some(healthy_owner.clone())));
    let global_owner: Option<String> =
        sqlx::query_scalar("SELECT lease_owner FROM media_reconciler_leases WHERE singleton = 1")
            .fetch_one(&context.state.pool)
            .await
            .expect("load active global lease");
    assert_eq!(global_owner.as_deref(), Some(healthy_owner.as_str()));

    let expired_at = Utc::now() - chrono::Duration::seconds(1);
    sqlx::query("UPDATE media_operations SET lease_expires_at = ? WHERE id = ?")
        .bind(expired_at)
        .bind(&operation_id)
        .execute(&context.state.pool)
        .await
        .expect("expire operation lease");
    assert_eq!(
        reconciliation::recover_interrupted_operations(&context.state.pool)
            .await
            .expect("recover expired operation"),
        1
    );
    let unknown = reconciliation::get_operation(&context.state.pool, &operation_id)
        .await
        .expect("load unknown operation");
    assert_eq!(unknown.state, "unknown");
    assert_eq!(unknown.error_code.as_deref(), Some("worker_lease_expired"));

    sqlx::query(
        "UPDATE media_reconciler_leases SET lease_expires_at = ?, updated_at = ? \
         WHERE singleton = 1",
    )
    .bind(expired_at)
    .bind(expired_at - chrono::Duration::minutes(1))
    .execute(&context.state.pool)
    .await
    .expect("expire global lease for takeover");

    assert!(reconciliation::reconcile_once(&context.state)
        .await
        .expect("safely retry idempotent desired state"));
    let succeeded = reconciliation::get_operation(&context.state.pool, &operation_id)
        .await
        .expect("load recovered operation");
    assert_eq!(succeeded.state, "succeeded");
    assert_eq!(succeeded.attempt, 2);
    context.state.pool.close().await;
}

#[tokio::test]
async fn media_claim_is_concurrent_safe_and_stable_state_is_idempotent() {
    let context = TestContext::current().await;
    let (_admin_id, admin) = context.bootstrap().await;
    let app = routes::router(context.state.clone());
    let response = send_request(
        &app,
        Method::POST,
        "/api/v2/cameras",
        &admin,
        Some(json!({
            "name": "Concurrent Camera",
            "main_stream_url": "rtsp://camera.example/concurrent",
            "enabled": true,
            "record_enabled": false
        })),
    )
    .await;
    let payload = response_json(response).await;
    let camera_id: Uuid = serde_json::from_value(payload["camera"]["id"].clone()).unwrap();

    let (first, second) = tokio::join!(
        reconciliation::reconcile_once(&context.state),
        reconciliation::reconcile_once(&context.state)
    );
    first.expect("first concurrent worker");
    second.expect("second concurrent worker");
    let calls_after_convergence = context.fake_media.mutation_calls();
    let operation_debug: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT id, state, reason, finished_at FROM media_operations WHERE camera_id = ? ORDER BY created_at, id",
    )
    .bind(camera_id)
    .fetch_all(&context.state.pool)
    .await
    .expect("load operation debug state");
    assert_eq!(
        calls_after_convergence, 2,
        "operations: {operation_debug:?}"
    );
    let succeeded: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM media_operations WHERE camera_id = ? AND state = 'succeeded'",
    )
    .bind(camera_id)
    .fetch_one(&context.state.pool)
    .await
    .expect("count completed operations");
    assert_eq!(succeeded, 1);

    assert!(!reconciliation::reconcile_once(&context.state)
        .await
        .expect("observe already converged state"));
    assert_eq!(context.fake_media.mutation_calls(), calls_after_convergence);

    let path = camera_path(camera_id, "main");
    context.fake_media.remove_path(&path);
    assert!(reconciliation::reconcile_once(&context.state)
        .await
        .expect("repair externally introduced drift"));
    assert!(context.fake_media.path(&path).is_some());
    let operations: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM media_operations WHERE camera_id = ?")
            .bind(camera_id)
            .fetch_one(&context.state.pool)
            .await
            .expect("count initial and drift operations");
    assert_eq!(operations, 2);
    context.state.pool.close().await;
}
