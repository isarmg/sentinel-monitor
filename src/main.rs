mod auth;
mod background;
mod config;
mod crypto;
mod doctor;
mod error;
mod mediamtx;
mod models;
mod onvif;
mod protocol;
mod reconciliation;
mod release;
mod routes;
mod runtime_lock;
mod sqlite;
mod static_assets;

#[cfg(test)]
static NETWORK_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

use base64::{engine::general_purpose::STANDARD, Engine as _};
use clap::{Args, Parser, Subcommand};
use config::Config;
use crypto::SecretBox;
use mediamtx::MediaMtxClient;
use models::EventRecord;
use sqlx::SqlitePool;
use std::{path::PathBuf, sync::Arc};
use tokio::{net::TcpListener, sync::broadcast};
use tracing_subscriber::EnvFilter;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub pool: SqlitePool,
    pub secrets: SecretBox,
    pub http: reqwest::Client,
    pub media: MediaMtxClient,
    pub events: broadcast::Sender<EventRecord>,
    pub administrator:
        Arc<sarmg_admin_core::AdministratorService<sarmg_admin_sqlite::SqliteAdministratorStore>>,
    pub administrator_origin: sarmg_admin_auth::AdministratorOriginMode,
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
    /// Serve only after verifying this exact physical 0.2.0 release tree.
    ServeRelease { release_root: PathBuf },
    /// Check database read/write, credential, storage, readiness and companion contracts.
    Doctor(DoctorArgs),
    /// Print the static asset contract embedded in this build.
    #[command(hide = true)]
    StaticContract,
    /// Print the complete identity compiled into this binary.
    ReleaseIdentity,
    /// Verify a complete physical 0.2.0 release with its own binary.
    VerifyRelease { release_root: PathBuf },
    /// Print the canonical release-manifest identity header.
    #[command(hide = true)]
    ReleaseManifestHeader,
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
        default_value = "http://127.0.0.1:8080/readyz"
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
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,tower_http=info")),
        )
        .init();

    match Cli::parse().command.unwrap_or(Command::Serve) {
        Command::Serve => {
            release::ensure_unbound_development_serve()?;
            serve(None).await
        }
        Command::ServeRelease { release_root } => {
            release::verify_release(&release_root)?;
            serve(Some(&release_root)).await
        }
        Command::Doctor(args) => {
            let key = required_credentials_key()?;
            let report = doctor::run(&doctor::DoctorOptions {
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
        Command::StaticContract => {
            println!("{}", static_assets::embedded_contract_sha256()?);
            Ok(())
        }
        Command::ReleaseIdentity => {
            println!("{}", release::identity_json()?);
            Ok(())
        }
        Command::VerifyRelease { release_root } => {
            println!(
                "{}",
                serde_json::to_string(&release::verify_release(&release_root)?)?
            );
            Ok(())
        }
        Command::ReleaseManifestHeader => {
            print!("{}", release::manifest_header()?);
            Ok(())
        }
    }
}

fn required_credentials_key() -> anyhow::Result<[u8; 32]> {
    let encoded = std::env::var("CREDENTIALS_KEY").map_err(|_| {
        anyhow::anyhow!("CREDENTIALS_KEY is required and must remain in secret management")
    })?;
    let decoded = STANDARD
        .decode(encoded)
        .map_err(|_| anyhow::anyhow!("CREDENTIALS_KEY must be valid base64"))?;
    decoded
        .try_into()
        .map_err(|_| anyhow::anyhow!("CREDENTIALS_KEY must decode to exactly 32 bytes"))
}

async fn serve(release_root: Option<&std::path::Path>) -> anyhow::Result<()> {
    let config =
        Arc::new(Config::from_env().map_err(|error| anyhow::anyhow!("configuration: {error}"))?);
    if let Some(root) = release_root {
        anyhow::ensure!(
            config.static_dir == root.join("web"),
            "STATIC_DIR must equal the verified physical release Web directory"
        );
    }
    static_assets::validate(&config.static_dir, !config.development_mode)
        .map_err(|error| anyhow::anyhow!("static asset contract: {error:#}"))?;
    let application_lock =
        runtime_lock::ApplicationLock::acquire(&config.database_url, &config.runtime_directory)?;
    if config.development_mode {
        tracing::warn!(
            address = %config.bind_addr,
            "development mode uses loopback-only cookies without Secure"
        );
    }
    let pool = sqlite::open_pool(&config.database_url).await?;
    application_lock.validate_open_database()?;

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
    let administrator = Arc::new(sarmg_admin_core::AdministratorService::new(
        sarmg_admin_sqlite::SqliteAdministratorStore::new(pool.clone()),
    ));
    use sarmg_admin_core::AdministratorStore as _;
    administrator.store().validate_all_administrators().await?;
    if administrator.store().administrator_count().await? == 0 {
        administrator
            .bootstrap_administrator(
                &config.bootstrap_admin_username,
                config.bootstrap_admin_password.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "BOOTSTRAP_ADMIN_PASSWORD is required while no administrators exist"
                    )
                })?,
                current_time_micros()?,
            )
            .await
            .map_err(|error| anyhow::anyhow!(error))?;
    }
    let administrator_origin = if config.development_mode {
        sarmg_admin_auth::AdministratorOriginMode::LoopbackDevelopmentHttp
    } else {
        sarmg_admin_auth::AdministratorOriginMode::ProductionHttps
    };
    let state = AppState {
        secrets: SecretBox::new(&config.credentials_key),
        config: config.clone(),
        pool,
        http,
        media,
        events,
        administrator,
        administrator_origin,
    };

    reconciliation::validate_stored_camera_credentials(&state).await?;
    let recovered = reconciliation::recover_interrupted_operations(&state.pool).await?;
    if recovered > 0 {
        tracing::warn!(
            recovered_operations = recovered,
            "expired media operation leases were marked unknown for safe reconciliation"
        );
    }
    let listener = TcpListener::bind(config.bind_addr).await?;
    let health_pool = state.pool.clone();
    let reconcile_state = state.clone();
    let status_state = state.clone();
    let audit_pool = state.pool.clone();
    let audit_delivery_pool = state.pool.clone();
    let operations_pool = state.pool.clone();
    let runtime =
        sarmg_server_runtime::ServerRuntime::builder(sarmg_server_runtime::ProductDescriptor {
            id: "sentinel-monitor".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            foundation_revision: "1e889d08fa69fcf2b5fffe45e8cc42b68218f4f1".into(),
            profile: "server-control-plane".into(),
            capabilities: vec![
                "admin-persistent".into(),
                "server-runtime".into(),
                "server-health".into(),
                "durable-operations".into(),
                "secure-http".into(),
                "secure-xml".into(),
                "secret-envelope".into(),
            ],
        })
        .with_schema_identity(sqlite::current_schema_identity()?)
        .register_metric(
            sarmg_server_runtime::DiagnosticMetric::AuditBacklog,
            move || {
                let store = sarmg_operations::SqliteOperationStore::new(audit_pool.clone());
                async move { store.pending_audit_count().await.ok() }
            },
        )
        .register_metric(
            sarmg_server_runtime::DiagnosticMetric::OperationBacklog,
            move || {
                let store = sarmg_operations::SqliteOperationStore::new(operations_pool.clone());
                async move { store.active_operation_count().await.ok() }
            },
        )
        .register_health_check(
            "database",
            sarmg_server_runtime::health_check(move || {
                let pool = health_pool.clone();
                async move {
                    sqlx::query_scalar::<_, i64>("SELECT 1")
                        .fetch_one(&pool)
                        .await
                        .is_ok_and(|value| value == 1)
                }
            }),
        )
        .register_background_task(
            "media-reconciliation",
            sarmg_server_runtime::TaskCriticality::Critical,
            move |shutdown| background::reconcile_loop(reconcile_state, shutdown),
        )
        .register_background_task(
            "operation-audit",
            sarmg_server_runtime::TaskCriticality::Degrading,
            move |shutdown| background::operation_audit_loop(audit_delivery_pool, shutdown),
        )
        .register_background_task(
            "camera-status",
            sarmg_server_runtime::TaskCriticality::Degrading,
            move |shutdown| background::status_loop(status_state, shutdown),
        )
        .build()
        .await?;
    let runtime_handle = runtime.handle();
    tracing::info!(address = %config.bind_addr, "sentinel monitor started");
    runtime
        .serve(listener, routes::router(state, runtime_handle)?)
        .await?;
    Ok(())
}

fn current_time_micros() -> anyhow::Result<u64> {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_micros(),
    )
    .map_err(|_| anyhow::anyhow!("current time exceeds administrator timestamp range"))
}
