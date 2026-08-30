use crate::{
    auth::{bootstrap_admin, issue_session},
    background::emit_event,
    config::Config,
    crypto::SecretBox,
    mediamtx::MediaMtxClient,
    models::CameraRecord,
    routes, AppState,
};
use axum::{
    body::{to_bytes, Body},
    http::{header::AUTHORIZATION, Method, Request, StatusCode},
    response::Response,
    Router,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    SqlitePool,
};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{sync::broadcast, task::JoinHandle};
use tower::ServiceExt;
use uuid::Uuid;

struct TestContext {
    database_path: PathBuf,
    state: AppState,
    media_task: JoinHandle<()>,
}

impl Drop for TestContext {
    fn drop(&mut self) {
        self.media_task.abort();
        let base = self.database_path.to_string_lossy();
        for path in [
            self.database_path.clone(),
            PathBuf::from(format!("{base}-shm")),
            PathBuf::from(format!("{base}-wal")),
        ] {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl TestContext {
    async fn migrated() -> Self {
        let database_path = std::env::temp_dir().join(format!(
            "sentinel-monitor-sqlite-test-{}.sqlite3",
            Uuid::new_v4()
        ));
        let options = SqliteConnectOptions::new()
            .filename(&database_path)
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
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
            database_url: format!("sqlite://{}", database_path.display()),
            jwt_secret: b"sentinel-test-jwt-secret-32-bytes".to_vec(),
            credentials_key: [7; 32],
            bootstrap_admin_email: "admin@example.com".into(),
            bootstrap_admin_password: Some("bootstrap-password".into()),
            session_cookie_secure: false,
            session_ttl: Duration::from_secs(3_600),
            media_token_ttl: Duration::from_secs(120),
            mediamtx_api_url: media_url.clone(),
            mediamtx_playback_url: media_url,
            public_webrtc_base_url: "/media-webrtc".into(),
            public_hls_base_url: "/media-hls".into(),
            status_interval: Duration::from_secs(10),
            reconcile_interval: Duration::from_secs(60),
            request_timeout: Duration::from_millis(100),
            onvif_discovery_timeout: Duration::from_millis(100),
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
            database_path,
            state,
            media_task,
        }
    }

    async fn bootstrap(&self) -> (Uuid, String) {
        bootstrap_admin(&self.state)
            .await
            .expect("bootstrap administrator");
        let id = sqlx::query_scalar::<_, Uuid>("SELECT id FROM users WHERE role = 'admin'")
            .fetch_one(&self.state.pool)
            .await
            .expect("load administrator id");
        let token = issue_session(id, &self.state.config).expect("issue administrator session");
        (id, token)
    }
}

async fn send_json(
    app: &Router,
    method: Method,
    uri: &str,
    token: &str,
    value: serde_json::Value,
) -> StatusCode {
    send_request(app, method, uri, token, Some(value))
        .await
        .status()
}

async fn send_request(
    app: &Router,
    method: Method,
    uri: &str,
    token: &str,
    value: Option<Value>,
) -> Response {
    let body = value
        .map(|value| Body::from(value.to_string()))
        .unwrap_or_else(Body::empty);
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(body)
        .expect("build test request");
    app.clone()
        .oneshot(request)
        .await
        .expect("serve test request")
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
