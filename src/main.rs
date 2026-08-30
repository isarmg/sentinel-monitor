mod auth;
mod background;
mod backup;
mod config;
mod crypto;
mod error;
mod login_security;
mod mediamtx;
mod models;
mod onvif;
mod reconciliation;
mod routes;
mod runtime_lock;
mod sqlite;

#[cfg(test)]
mod sqlite_tests;

#[cfg(test)]
static NETWORK_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

use clap::{Args, Parser, Subcommand};
use config::Config;
use crypto::SecretBox;
use login_security::LoginProtection;
use mediamtx::MediaMtxClient;
use models::EventRecord;
use sqlx::SqlitePool;
use std::{path::PathBuf, sync::Arc};
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

#[derive(Parser)]
#[command(
    name = "sentinel-monitor",
    version,
    about = "Sentinel camera monitoring control plane"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the HTTP control plane (the default command).
    Serve,
    /// Create or verify a complete Sentinel backup bundle.
    Backup {
        #[command(subcommand)]
        command: BackupCommand,
    },
    /// Atomically restore a verified bundle while Sentinel and MediaMTX are stopped.
    Restore(RestoreArgs),
    /// Check database read/write, credential, storage, readiness and companion contracts.
    Doctor(DoctorArgs),
}

#[derive(Subcommand)]
enum BackupCommand {
    /// Create a non-overwriting SQLite, MediaMTX and recording backup bundle.
    Create(CreateBackupArgs),
    /// Verify hashes, schema, foreign keys, recording inventory and a temporary restore.
    Verify(VerifyBackupArgs),
}

#[derive(Args)]
struct CreateBackupArgs {
    #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
    database_url: String,
    #[arg(long)]
    output: PathBuf,
    #[command(flatten)]
    media: MediaPaths,
    #[arg(long, env = "SENTINEL_RUNTIME_DIR")]
    runtime_dir: PathBuf,
    /// Non-secret identifier for the separately escrowed CREDENTIALS_KEY.
    #[arg(long, env = "CREDENTIALS_KEY_ID", hide_env_values = true)]
    credentials_key_id: String,
}

#[derive(Args)]
struct VerifyBackupArgs {
    #[arg(long)]
    input: PathBuf,
}

#[derive(Args)]
struct RestoreArgs {
    #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
    database_url: String,
    #[arg(long)]
    input: PathBuf,
    #[command(flatten)]
    media: MediaPaths,
    #[arg(long, env = "SENTINEL_RUNTIME_DIR")]
    runtime_dir: PathBuf,
    /// Non-secret identifier for the separately escrowed CREDENTIALS_KEY.
    #[arg(long, env = "CREDENTIALS_KEY_ID", hide_env_values = true)]
    credentials_key_id: String,
}

#[derive(Args)]
struct DoctorArgs {
    #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
    database_url: String,
    #[command(flatten)]
    media: MediaPaths,
    /// Skip live loopback HTTP probes; storage and companion checks still run.
    #[arg(long)]
    offline: bool,
    #[arg(
        long,
        env = "SENTINEL_READY_URL",
        default_value = "http://127.0.0.1:8080/health/ready"
    )]
    app_ready_url: String,
    #[arg(
        long,
        env = "MEDIAMTX_READY_URL",
        default_value = "http://127.0.0.1:9997/v3/info"
    )]
    mediamtx_ready_url: String,
}

#[derive(Args)]
struct MediaPaths {
    #[arg(long, env = "MEDIAMTX_CONFIG")]
    mediamtx_config: PathBuf,
    #[arg(long, env = "MEDIAMTX_CONTRACT")]
    mediamtx_contract: PathBuf,
    #[arg(long, env = "MEDIAMTX_BINARY")]
    mediamtx_binary: PathBuf,
    #[arg(long, env = "RECORDINGS_DIR")]
    recordings_dir: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,tower_http=info")),
        )
        .init();

    match Cli::parse().command.unwrap_or(Command::Serve) {
        Command::Serve => serve().await,
        Command::Backup {
            command: BackupCommand::Create(args),
        } => {
            let key = required_credentials_key()?;
            let manifest = backup::create(&backup::CreateOptions {
                database_url: args.database_url,
                output: args.output,
                mediamtx_config: args.media.mediamtx_config,
                mediamtx_contract: args.media.mediamtx_contract,
                mediamtx_binary: args.media.mediamtx_binary,
                recordings_directory: args.media.recordings_dir,
                runtime_directory: args.runtime_dir,
                credentials_key_id: args.credentials_key_id,
                credentials_key: key,
            })?;
            println!(
                "{}",
                serde_json::json!({
                    "status": "backup-created",
                    "application": manifest.application,
                    "schema": manifest.database_schema,
                    "recording_files": manifest.data_files,
                    "recording_bytes": manifest.data_bytes
                })
            );
            Ok(())
        }
        Command::Backup {
            command: BackupCommand::Verify(args),
        } => {
            let manifest = backup::verify(&args.input)?;
            println!(
                "{}",
                serde_json::json!({
                    "status": "backup-verified",
                    "application": manifest.application,
                    "schema": manifest.database_schema,
                    "recording_files": manifest.data_files,
                    "recording_bytes": manifest.data_bytes
                })
            );
            Ok(())
        }
        Command::Restore(args) => {
            let key = required_credentials_key()?;
            let manifest = backup::restore(&backup::RestoreOptions {
                database_url: args.database_url,
                input: args.input,
                mediamtx_config: args.media.mediamtx_config,
                mediamtx_contract: args.media.mediamtx_contract,
                mediamtx_binary: args.media.mediamtx_binary,
                recordings_directory: args.media.recordings_dir,
                runtime_directory: args.runtime_dir,
                credentials_key_id: args.credentials_key_id,
                credentials_key: key,
            })?;
            println!(
                "{}",
                serde_json::json!({
                    "status": "restored",
                    "application": manifest.application,
                    "schema": manifest.database_schema,
                    "recording_files": manifest.data_files
                })
            );
            Ok(())
        }
        Command::Doctor(args) => {
            let key = required_credentials_key()?;
            let report = backup::doctor(&backup::DoctorOptions {
                database_url: args.database_url,
                mediamtx_config: args.media.mediamtx_config,
                mediamtx_contract: args.media.mediamtx_contract,
                mediamtx_binary: args.media.mediamtx_binary,
                recordings_directory: args.media.recordings_dir,
                credentials_key: key,
                app_ready_url: args.app_ready_url,
                mediamtx_ready_url: args.mediamtx_ready_url,
                offline: args.offline,
            })
            .await?;
            println!("{}", serde_json::to_string(&report)?);
            Ok(())
        }
    }
}

fn required_credentials_key() -> anyhow::Result<[u8; 32]> {
    let encoded = std::env::var("CREDENTIALS_KEY").map_err(|_| {
        anyhow::anyhow!("CREDENTIALS_KEY is required and must remain outside backups")
    })?;
    backup::credentials_key_from_base64(&encoded)
}

async fn serve() -> anyhow::Result<()> {
    let config =
        Arc::new(Config::from_env().map_err(|error| anyhow::anyhow!("configuration: {error}"))?);
    let _application_lock = runtime_lock::ApplicationLock::acquire(&config.runtime_directory)?;
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
    let recovered = reconciliation::recover_interrupted_operations(&state.pool).await?;
    if recovered > 0 {
        tracing::warn!(
            recovered_operations = recovered,
            "expired media operation leases were marked unknown for safe reconciliation"
        );
    }
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
