use crate::{
    auth::{bootstrap_admin, issue_session},
    background::emit_event,
    config::Config,
    crypto::SecretBox,
    mediamtx::MediaMtxClient,
    routes, AppState,
};
use axum::{
    body::Body,
    http::{header::AUTHORIZATION, Method, Request, StatusCode},
    Router,
};
use chrono::DateTime;
use serde_json::json;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    SqlitePool,
};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::broadcast;
use tower::ServiceExt;
use uuid::Uuid;

struct TestContext {
    database_path: PathBuf,
    state: AppState,
}

impl Drop for TestContext {
    fn drop(&mut self) {
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
            mediamtx_api_url: "http://127.0.0.1:1".into(),
            mediamtx_playback_url: "http://127.0.0.1:1".into(),
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
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(value.to_string()))
        .expect("build test request");
    app.clone()
        .oneshot(request)
        .await
        .expect("serve test request")
        .status()
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
