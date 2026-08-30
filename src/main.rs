mod auth;
mod background;
mod config;
mod crypto;
mod error;
mod login_security;
mod mediamtx;
mod models;
mod onvif;
mod routes;
mod sqlite;

#[cfg(test)]
mod sqlite_tests;

#[cfg(test)]
static NETWORK_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

use config::Config;
use crypto::SecretBox;
use login_security::LoginProtection;
use mediamtx::MediaMtxClient;
use models::EventRecord;
use sqlx::SqlitePool;
use std::{error::Error, sync::Arc};
use tokio::{net::TcpListener, signal, sync::broadcast};
use tracing_subscriber::EnvFilter;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub pool: SqlitePool,
    pub secrets: SecretBox,
    pub http: reqwest::Client,
    pub media: MediaMtxClient,
    pub events: broadcast::Sender<EventRecord>,
    pub login: LoginProtection,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,tower_http=info")),
        )
        .init();

    let config = Arc::new(Config::from_env().map_err(|error| format!("configuration: {error}"))?);
    if config.development_mode {
        tracing::warn!(
            address = %config.bind_addr,
            "development mode uses loopback-only cookies without Secure"
        );
    }
    let pool = sqlite::open_pool(&config.database_url).await?;
    sqlx::migrate!().run(&pool).await?;

    let http = reqwest::Client::builder()
        .timeout(config.request_timeout)
        .user_agent(concat!("sentinel-monitor/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let media = MediaMtxClient::new(
        http.clone(),
        config.mediamtx_api_url.clone(),
        config.mediamtx_playback_url.clone(),
    );
    let (events, _) = broadcast::channel(256);
    let state = AppState {
        secrets: SecretBox::new(&config.credentials_key),
        config: config.clone(),
        pool,
        http,
        media,
        events,
        login: LoginProtection::new(&config),
    };

    auth::bootstrap_admin(&state).await?;
    background::spawn(state.clone());

    let listener = TcpListener::bind(config.bind_addr).await?;
    tracing::info!(address = %config.bind_addr, "sentinel monitor started");
    axum::serve(
        listener,
        routes::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown requested");
}
