mod auth;
mod background;
mod config;
mod crypto;
mod error;
mod mediamtx;
mod models;
mod onvif;
mod routes;

use config::Config;
use crypto::SecretBox;
use mediamtx::MediaMtxClient;
use models::EventRecord;
use sarmg_platform_postgres::{connect, run_migrations, PostgresConfig};
use sqlx::PgPool;
use std::{error::Error, sync::Arc};
use tokio::{net::TcpListener, signal, sync::broadcast};
use tracing_subscriber::EnvFilter;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub pool: PgPool,
    pub secrets: SecretBox,
    pub http: reqwest::Client,
    pub media: MediaMtxClient,
    pub events: broadcast::Sender<EventRecord>,
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
    let pool = connect(&PostgresConfig::new(&config.database_url)).await?;
    run_migrations(&pool, &sqlx::migrate!()).await?;

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
    };

    auth::bootstrap_admin(&state).await?;
    background::spawn(state.clone());

    let listener = TcpListener::bind(config.bind_addr).await?;
    tracing::info!(address = %config.bind_addr, "sentinel monitor started");
    axum::serve(listener, routes::router(state))
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
