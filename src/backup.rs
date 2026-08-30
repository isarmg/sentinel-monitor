use crate::{crypto::SecretBox, runtime_lock::DatabaseMaintenanceLock};
use anyhow::{bail, ensure, Context};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::{DateTime, Utc};
use rusqlite::{backup::Backup as SqliteBackup, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{BufReader, BufWriter, Read, Write},
    path::{Component, Path, PathBuf},
    process::Command,
    time::Duration,
};
use tempfile::TempDir;
use uuid::Uuid;

#[cfg(unix)]
use std::os::{
    fd::AsRawFd,
    unix::fs::{MetadataExt, OpenOptionsExt},
};

const APPLICATION: &str = "sentinel-monitor";
const FORMAT_VERSION: u32 = 1;
const DATABASE_FILE: &str = "database.sqlite3";
const MEDIA_CONFIG_FILE: &str = "mediamtx.yml";
const MEDIA_CONTRACT_FILE: &str = "mediamtx.lock";
const MANIFEST_FILE: &str = "manifest.json";
const RECORDINGS_DIRECTORY: &str = "recordings";
const CURRENT_SCHEMA_VERSION: i64 = 3;
const REQUIRED_TABLES: [&str; 9] = [
    "users",
    "cameras",
    "events",
    "audit_logs",
    "browser_sessions",
    "media_desired_states",
    "media_operations",
    "media_actual_paths",
    "media_reconciler_leases",
];
const REQUIRED_INDEXES: [&str; 14] = [
    "users_email_lower_idx",
    "cameras_status_idx",
    "cameras_enabled_idx",
    "events_created_at_idx",
    "events_camera_id_idx",
    "events_unacknowledged_idx",
    "audit_logs_created_at_idx",
    "browser_sessions_user_idx",
    "browser_sessions_expiry_idx",
    "media_operations_queue_idx",
    "media_operations_camera_idx",
    "media_operations_active_generation_idx",
    "media_actual_paths_camera_idx",
    "sqlite_autoindex_media_desired_states_1",
];

#[derive(Clone, Debug)]
pub struct CreateOptions {
    pub database_url: String,
    pub output: PathBuf,
    pub mediamtx_config: PathBuf,
    pub mediamtx_contract: PathBuf,
    pub mediamtx_binary: PathBuf,
    pub recordings_directory: PathBuf,
    pub runtime_directory: PathBuf,
    pub credentials_key_id: String,
    pub credentials_key: [u8; 32],
}

#[derive(Clone, Debug)]
pub struct RestoreOptions {
    pub database_url: String,
    pub input: PathBuf,
    pub mediamtx_config: PathBuf,
    pub mediamtx_contract: PathBuf,
    pub mediamtx_binary: PathBuf,
    pub recordings_directory: PathBuf,
    pub runtime_directory: PathBuf,
    pub credentials_key_id: String,
    pub credentials_key: [u8; 32],
}

#[derive(Clone, Debug)]
pub struct DoctorOptions {
    pub database_url: String,
    pub mediamtx_config: PathBuf,
    pub mediamtx_contract: PathBuf,
    pub mediamtx_binary: PathBuf,
    pub recordings_directory: PathBuf,
    pub credentials_key: [u8; 32],
    pub app_ready_url: String,
    pub mediamtx_ready_url: String,
    pub offline: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct DoctorReport {
    pub status: &'static str,
    pub database_read: bool,
    pub database_write: bool,
    pub credential_decryption: bool,
    pub recording_storage_read_write: bool,
    pub companion_contract: bool,
    pub application_ready: Option<bool>,
    pub mediamtx_ready: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupManifest {
    pub format_version: u32,
    pub application: String,
    pub application_version: String,
    pub database_schema: i64,
    pub created_at: DateTime<Utc>,
    pub database: StoredFile,
    pub database_records: BTreeMap<String, u64>,
    pub mediamtx_config: StoredFile,
    pub mediamtx_contract: CompanionContract,
    pub recordings: RecordingArchive,
    pub data_files: u64,
    pub data_bytes: u64,
    pub credentials_key: CredentialsKeyRequirement,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredFile {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompanionContract {
    pub contract_file: StoredFile,
    pub version: String,
    pub platform: String,
    pub binary_sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingArchive {
    pub directory: String,
    pub files: Vec<StoredFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialsKeyRequirement {
    pub included: bool,
    pub required_for_restore: bool,
    pub key_id: String,
}

#[derive(Debug)]
struct DatabaseSummary {
    schema_version: i64,
    records: BTreeMap<String, u64>,
}

#[derive(Clone, Debug)]
struct ParsedContract {
    version: String,
    platform: String,
    sha256: String,
}

pub fn credentials_key_from_base64(value: &str) -> anyhow::Result<[u8; 32]> {
    let decoded = STANDARD
        .decode(value)
        .context("CREDENTIALS_KEY must be valid base64")?;
    decoded
        .try_into()
        .map_err(|_| anyhow::anyhow!("CREDENTIALS_KEY must decode to exactly 32 bytes"))
}

pub fn create(options: &CreateOptions) -> anyhow::Result<BackupManifest> {
    validate_key_id(&options.credentials_key_id)?;
    let _service_locks = ServiceLocks::acquire(&options.database_url, &options.runtime_directory)?;
    validate_source_layout(
        &options.mediamtx_config,
        &options.mediamtx_contract,
        &options.mediamtx_binary,
        &options.recordings_directory,
    )?;

    let database_path = database_path(&options.database_url)?;
    require_regular_file_without_symlinks(&database_path, "SQLite database")?;
    let contract = verify_companion(
        &options.mediamtx_contract,
        &options.mediamtx_binary,
        &options.mediamtx_config,
        &options.recordings_directory,
    )?;
    let source_summary = verify_database(&database_path)?;
    verify_credentials(&database_path, &options.credentials_key)?;

    reject_symlink_components(&options.output)?;
    ensure_backup_output_disjoint(
        &options.output,
        &database_path,
        &options.mediamtx_config,
        &options.mediamtx_contract,
        &options.mediamtx_binary,
        &options.recordings_directory,
    )?;
    let mut pending = PendingDirectory::create(&options.output)?;
    let database_output = options.output.join(DATABASE_FILE);
    copy_database_online(&database_path, &database_output)?;
    let database_summary = verify_database(&database_output)?;
    ensure!(
        database_summary.records == source_summary.records,
        "database changed after services were stopped; refusing an inconsistent backup"
    );
    verify_credentials(&database_output, &options.credentials_key)?;

    let config_output = options.output.join(MEDIA_CONFIG_FILE);
    let config_file = copy_regular_file(&options.mediamtx_config, &config_output)?;
    let contract_output = options.output.join(MEDIA_CONTRACT_FILE);
    let contract_file = copy_regular_file(&options.mediamtx_contract, &contract_output)?;
    let recordings_output = options.output.join(RECORDINGS_DIRECTORY);
    create_private_directory(&recordings_output)?;
    let recording_files =
        copy_recording_tree(&options.recordings_directory, &recordings_output, None)?;
    let (data_files, data_bytes) = recording_totals(&recording_files)?;

    let manifest = BackupManifest {
        format_version: FORMAT_VERSION,
        application: APPLICATION.to_string(),
        application_version: env!("CARGO_PKG_VERSION").to_string(),
        database_schema: database_summary.schema_version,
        created_at: Utc::now(),
        database: stored_file(DATABASE_FILE, &database_output)?,
        database_records: database_summary.records,
        mediamtx_config: StoredFile {
            path: MEDIA_CONFIG_FILE.to_string(),
            ..config_file
        },
        mediamtx_contract: CompanionContract {
            contract_file: StoredFile {
                path: MEDIA_CONTRACT_FILE.to_string(),
                ..contract_file
            },
            version: contract.version,
            platform: contract.platform,
            binary_sha256: contract.sha256,
        },
        recordings: RecordingArchive {
            directory: RECORDINGS_DIRECTORY.to_string(),
            files: recording_files,
        },
        data_files,
        data_bytes,
        credentials_key: CredentialsKeyRequirement {
            included: false,
            required_for_restore: true,
            key_id: options.credentials_key_id.clone(),
        },
    };
    write_manifest(&options.output.join(MANIFEST_FILE), &manifest)?;
    sync_tree(&options.output)?;
    verify(&options.output)?;
    sync_parent(&options.output)?;
    pending.commit();
    Ok(manifest)
}

pub fn verify(input: &Path) -> anyhow::Result<BackupManifest> {
    require_directory_without_symlinks(input, "backup bundle")?;
    let manifest_path = input.join(MANIFEST_FILE);
    require_regular_file_without_symlinks(&manifest_path, "backup manifest")?;
    let bytes = read_limited(&manifest_path, 4 * 1024 * 1024)?;
    let manifest: BackupManifest =
        serde_json::from_slice(&bytes).context("parse backup manifest")?;
    validate_manifest(&manifest)?;
    validate_bundle_inventory(input, &manifest)?;

    verify_stored_file(input, &manifest.database)?;
    verify_stored_file(input, &manifest.mediamtx_config)?;
    verify_stored_file(input, &manifest.mediamtx_contract.contract_file)?;
    for file in &manifest.recordings.files {
        verify_stored_file(&input.join(RECORDINGS_DIRECTORY), file)?;
    }

    let contract_path = input.join(&manifest.mediamtx_contract.contract_file.path);
    let parsed = parse_contract(&contract_path)?;
    ensure!(
        parsed.version == manifest.mediamtx_contract.version
            && parsed.platform == manifest.mediamtx_contract.platform
            && parsed.sha256 == manifest.mediamtx_contract.binary_sha256,
        "MediaMTX contract does not match the manifest"
    );
    verify_media_config(&input.join(&manifest.mediamtx_config.path), None)?;

    let temporary = TempDir::new().context("create temporary verification directory")?;
    let restored_database = temporary.path().join(DATABASE_FILE);
    copy_database_online(&input.join(&manifest.database.path), &restored_database)?;
    let summary = verify_database(&restored_database)?;
    ensure!(
        summary.schema_version == manifest.database_schema,
        "restored schema version does not match the manifest"
    );
    ensure!(
        summary.records == manifest.database_records,
        "restored database record counts do not match the manifest"
    );
    let (files, bytes) = recording_totals(&manifest.recordings.files)?;
    ensure!(
        files == manifest.data_files && bytes == manifest.data_bytes,
        "recording totals do not match the manifest"
    );
    Ok(manifest)
}

pub fn restore(options: &RestoreOptions) -> anyhow::Result<BackupManifest> {
    restore_inner(options, None)
}

fn restore_inner(
    options: &RestoreOptions,
    fail_after_install: Option<usize>,
) -> anyhow::Result<BackupManifest> {
    validate_key_id(&options.credentials_key_id)?;
    let manifest = verify(&options.input)?;
    ensure!(
        manifest.credentials_key.key_id == options.credentials_key_id,
        "the supplied credentials key ID does not match this backup"
    );
    let _service_locks = ServiceLocks::acquire(&options.database_url, &options.runtime_directory)?;
    let deployed_contract =
        verify_binary_contract(&options.mediamtx_contract, &options.mediamtx_binary)?;
    ensure!(
        deployed_contract.version == manifest.mediamtx_contract.version
            && deployed_contract.platform == manifest.mediamtx_contract.platform
            && deployed_contract.sha256 == manifest.mediamtx_contract.binary_sha256,
        "the deployed MediaMTX companion does not match this backup"
    );

    let destination_database = database_path(&options.database_url)?;
    validate_restore_destinations(
        &options.input,
        &destination_database,
        &options.mediamtx_config,
        &options.recordings_directory,
    )?;
    let database_stage = adjacent_temporary(&destination_database, "database")?;
    let config_stage = adjacent_temporary(&options.mediamtx_config, "config")?;
    let recordings_stage = adjacent_temporary(&options.recordings_directory, "recordings")?;
    let mut staged = StagedPaths::new(vec![
        database_stage.clone(),
        config_stage.clone(),
        recordings_stage.clone(),
    ]);

    copy_database_online(
        &options.input.join(&manifest.database.path),
        &database_stage,
    )?;
    verify_database_against_manifest(&database_stage, &manifest)?;
    verify_credentials(&database_stage, &options.credentials_key)?;
    copy_regular_file(
        &options.input.join(&manifest.mediamtx_config.path),
        &config_stage,
    )?;
    verify_media_config(&config_stage, Some(&options.recordings_directory))?;
    create_private_directory(&recordings_stage)?;
    copy_recording_tree(
        &options.input.join(RECORDINGS_DIRECTORY),
        &recordings_stage,
        Some(&manifest.recordings.files),
    )?;
    verify_recording_tree(&recordings_stage, &manifest.recordings.files)?;
    sync_tree(&recordings_stage)?;

    let destination_lock = lock_destination_database(&destination_database)?;
    remove_sqlite_sidecars(&destination_database)?;
    let mut replacements = vec![
        Replacement::new(destination_database.clone(), database_stage),
        Replacement::new(options.mediamtx_config.clone(), config_stage),
        Replacement::new(options.recordings_directory.clone(), recordings_stage),
    ];
    let install_result = install_replacements(&mut replacements, fail_after_install);
    if let Err(error) = install_result {
        rollback_replacements(&mut replacements)?;
        drop(destination_lock);
        return Err(error);
    }

    let installed_result = (|| {
        verify_database_against_manifest(&destination_database, &manifest)?;
        verify_credentials(&destination_database, &options.credentials_key)?;
        verify_exact_file(&options.mediamtx_config, &manifest.mediamtx_config)?;
        verify_media_config(
            &options.mediamtx_config,
            Some(&options.recordings_directory),
        )?;
        verify_recording_tree(&options.recordings_directory, &manifest.recordings.files)?;
        sync_parent(&destination_database)?;
        sync_parent(&options.mediamtx_config)?;
        sync_parent(&options.recordings_directory)?;
        Ok::<_, anyhow::Error>(())
    })();
    if let Err(error) = installed_result {
        rollback_replacements(&mut replacements)?;
        drop(destination_lock);
        return Err(error.context("verify installed restore; original data was restored"));
    }

    drop(destination_lock);
    remove_sqlite_sidecars(&destination_database)?;
    cleanup_replacements(&mut replacements)?;
    staged.commit();
    Ok(manifest)
}

pub async fn doctor(options: &DoctorOptions) -> anyhow::Result<DoctorReport> {
    let _maintenance = DatabaseMaintenanceLock::shared(&options.database_url)?;
    let database = database_path(&options.database_url)?;
    require_regular_file_without_symlinks(&database, "SQLite database")?;
    verify_database(&database)?;
    verify_credentials(&database, &options.credentials_key)?;
    database_write_probe(&database)?;
    verify_companion(
        &options.mediamtx_contract,
        &options.mediamtx_binary,
        &options.mediamtx_config,
        &options.recordings_directory,
    )?;
    recording_write_probe(&options.recordings_directory)?;

    let (application_ready, mediamtx_ready) = if options.offline {
        (None, None)
    } else {
        let app = live_probe(&options.app_ready_url).await?;
        let media = live_probe(&options.mediamtx_ready_url).await?;
        ensure!(app, "application readiness endpoint is unavailable");
        ensure!(media, "MediaMTX readiness endpoint is unavailable");
        (Some(true), Some(true))
    };

    Ok(DoctorReport {
        status: "ok",
        database_read: true,
        database_write: true,
        credential_decryption: true,
        recording_storage_read_write: true,
        companion_contract: true,
        application_ready,
        mediamtx_ready,
    })
}

async fn live_probe(url: &str) -> anyhow::Result<bool> {
    let parsed = url::Url::parse(url).context("parse readiness URL")?;
    ensure!(
        matches!(parsed.scheme(), "http" | "https"),
        "readiness URL must use HTTP or HTTPS"
    );
    ensure!(
        parsed.username().is_empty() && parsed.password().is_none(),
        "readiness URL must not contain credentials"
    );
    ensure!(
        parsed.query().is_none() && parsed.fragment().is_none(),
        "readiness URL must not contain a query or fragment"
    );
    let loopback = match parsed.host() {
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        None => false,
    };
    ensure!(loopback, "readiness URL must target loopback");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()?;
    Ok(client.get(parsed).send().await?.status().is_success())
}

fn validate_manifest(manifest: &BackupManifest) -> anyhow::Result<()> {
    ensure!(
        manifest.format_version == FORMAT_VERSION,
        "unsupported backup format version"
    );
    ensure!(
        manifest.application == APPLICATION,
        "backup belongs to a different application"
    );
    ensure!(
        !manifest.application_version.trim().is_empty(),
        "backup application version is missing"
    );
    ensure!(
        manifest.database_schema == CURRENT_SCHEMA_VERSION,
        "backup schema is not supported by this application version"
    );
    ensure!(
        manifest.database.path == DATABASE_FILE,
        "backup database path is invalid"
    );
    ensure!(
        manifest.mediamtx_config.path == MEDIA_CONFIG_FILE,
        "backup MediaMTX config path is invalid"
    );
    ensure!(
        manifest.mediamtx_contract.contract_file.path == MEDIA_CONTRACT_FILE,
        "backup MediaMTX contract path is invalid"
    );
    ensure!(
        manifest.recordings.directory == RECORDINGS_DIRECTORY,
        "backup recordings path is invalid"
    );
    ensure!(
        !manifest.credentials_key.included && manifest.credentials_key.required_for_restore,
        "backup must require, but never contain, the credentials master key"
    );
    validate_key_id(&manifest.credentials_key.key_id)?;
    validate_sha256(&manifest.database.sha256)?;
    validate_sha256(&manifest.mediamtx_config.sha256)?;
    validate_sha256(&manifest.mediamtx_contract.contract_file.sha256)?;
    validate_sha256(&manifest.mediamtx_contract.binary_sha256)?;
    ensure!(
        manifest.mediamtx_contract.platform == "linux_amd64",
        "unsupported MediaMTX platform"
    );
    ensure!(
        !manifest.mediamtx_contract.version.trim().is_empty(),
        "MediaMTX version is missing"
    );
    let mut paths = BTreeSet::new();
    for file in &manifest.recordings.files {
        validate_relative_path(&file.path)?;
        validate_sha256(&file.sha256)?;
        ensure!(
            paths.insert(file.path.clone()),
            "recording manifest contains duplicate paths"
        );
    }
    let expected_tables: BTreeSet<_> = REQUIRED_TABLES.into_iter().map(str::to_string).collect();
    ensure!(
        manifest
            .database_records
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>()
            == expected_tables,
        "database record-count manifest is incomplete"
    );
    Ok(())
}

fn validate_bundle_inventory(root: &Path, manifest: &BackupManifest) -> anyhow::Result<()> {
    let mut expected = BTreeSet::from([
        MANIFEST_FILE.to_string(),
        DATABASE_FILE.to_string(),
        MEDIA_CONFIG_FILE.to_string(),
        MEDIA_CONTRACT_FILE.to_string(),
    ]);
    expected.extend(
        manifest
            .recordings
            .files
            .iter()
            .map(|file| format!("{RECORDINGS_DIRECTORY}/{}", file.path)),
    );
    let mut expected_directories = BTreeSet::from([RECORDINGS_DIRECTORY.to_string()]);
    expected_directories.extend(
        recording_parent_directories(&manifest.recordings.files)
            .into_iter()
            .map(|path| format!("{RECORDINGS_DIRECTORY}/{path}")),
    );
    let mut actual = BTreeSet::new();
    let mut actual_directories = BTreeSet::new();
    collect_bundle_files(root, root, &mut actual, &mut actual_directories)?;
    ensure!(
        actual == expected && actual_directories == expected_directories,
        "backup bundle has missing or unexpected files"
    );
    Ok(())
}

fn collect_bundle_files(
    root: &Path,
    directory: &Path,
    output: &mut BTreeSet<String>,
    directories: &mut BTreeSet<String>,
) -> anyhow::Result<()> {
    for entry in sorted_directory_entries(directory)? {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "backup bundle must not contain symbolic links"
        );
        if metadata.is_dir() {
            directories.insert(portable_relative_path(path.strip_prefix(root)?)?);
            collect_bundle_files(root, &path, output, directories)?;
        } else if metadata.is_file() {
            let relative = path.strip_prefix(root).expect("entry beneath bundle root");
            let relative = portable_relative_path(relative)?;
            output.insert(relative);
        } else {
            bail!("backup bundle must contain only regular files and directories");
        }
    }
    Ok(())
}

fn verify_database(path: &Path) -> anyhow::Result<DatabaseSummary> {
    require_regular_file_without_symlinks(path, "SQLite database")?;
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .context("open SQLite database read-only")?;
    connection.busy_timeout(Duration::from_secs(5))?;

    let integrity = pragma_rows(&connection, "PRAGMA integrity_check")?;
    ensure!(
        integrity.len() == 1 && integrity[0].eq_ignore_ascii_case("ok"),
        "SQLite integrity check failed"
    );
    let mut foreign_keys = connection.prepare("PRAGMA foreign_key_check")?;
    ensure!(
        foreign_keys.query([])?.next()?.is_none(),
        "SQLite foreign-key check failed"
    );

    let schema_version: Option<i64> = connection
        .query_row(
            "SELECT MAX(version) FROM _sqlx_migrations WHERE success = 1",
            [],
            |row| row.get(0),
        )
        .context("read SQLx migration version")?;
    let schema_version = schema_version.context("database has no successful migrations")?;
    ensure!(
        schema_version == CURRENT_SCHEMA_VERSION,
        "database schema version is unsupported"
    );
    let migration_versions: String = connection.query_row(
        "SELECT GROUP_CONCAT(version, ',') FROM (\
             SELECT version FROM _sqlx_migrations WHERE success = 1 ORDER BY version\
         )",
        [],
        |row| row.get(0),
    )?;
    ensure!(
        migration_versions == "1,2,3",
        "database migration history is incomplete"
    );
    let failed_migrations: i64 = connection.query_row(
        "SELECT COUNT(*) FROM _sqlx_migrations WHERE success = 0",
        [],
        |row| row.get(0),
    )?;
    ensure!(
        failed_migrations == 0,
        "database contains a failed migration"
    );

    for table in REQUIRED_TABLES {
        let present: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?",
            [table],
            |row| row.get(0),
        )?;
        ensure!(present == 1, "database is missing a required table");
    }
    for index in REQUIRED_INDEXES {
        let present: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name = ?",
            [index],
            |row| row.get(0),
        )?;
        ensure!(present == 1, "database is missing a required index");
    }
    verify_critical_columns(&connection)?;

    let mut records = BTreeMap::new();
    for table in REQUIRED_TABLES {
        let sql = format!("SELECT COUNT(*) FROM \"{table}\"");
        let count: i64 = connection.query_row(&sql, [], |row| row.get(0))?;
        ensure!(count >= 0, "database returned an invalid record count");
        records.insert(table.to_string(), count as u64);
    }
    Ok(DatabaseSummary {
        schema_version,
        records,
    })
}

fn verify_critical_columns(connection: &Connection) -> anyhow::Result<()> {
    let camera_columns = column_names(connection, "cameras")?;
    for column in [
        "main_stream_url_enc",
        "password_enc",
        "record_enabled",
        "deleted_at",
    ] {
        ensure!(
            camera_columns.contains(column),
            "camera schema is incomplete"
        );
    }
    let desired_columns = column_names(connection, "media_desired_states")?;
    for column in ["generation", "desired_present", "record_enabled"] {
        ensure!(
            desired_columns.contains(column),
            "media desired-state schema is incomplete"
        );
    }
    let operation_columns = column_names(connection, "media_operations")?;
    for column in [
        "generation",
        "state",
        "retry_at",
        "lease_owner",
        "lease_expires_at",
    ] {
        ensure!(
            operation_columns.contains(column),
            "media operation schema is incomplete"
        );
    }
    let actual_columns = column_names(connection, "media_actual_paths")?;
    for column in [
        "recording_active",
        "source_digest",
        "applied_generation",
        "observed_at",
    ] {
        ensure!(
            actual_columns.contains(column),
            "media actual-state schema is incomplete"
        );
    }
    Ok(())
}

fn column_names(connection: &Connection, table: &str) -> anyhow::Result<BTreeSet<String>> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
    rows.collect::<Result<_, _>>().map_err(Into::into)
}

fn pragma_rows(connection: &Connection, sql: &str) -> anyhow::Result<Vec<String>> {
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect::<Result<_, _>>().map_err(Into::into)
}

fn verify_database_against_manifest(path: &Path, manifest: &BackupManifest) -> anyhow::Result<()> {
    let summary = verify_database(path)?;
    ensure!(
        summary.schema_version == manifest.database_schema,
        "database schema does not match the backup manifest"
    );
    ensure!(
        summary.records == manifest.database_records,
        "database record counts do not match the backup manifest"
    );
    Ok(())
}

fn verify_credentials(path: &Path, key: &[u8; 32]) -> anyhow::Result<()> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(Duration::from_secs(5))?;
    let secret_box = SecretBox::new(key);
    let mut statement = connection
        .prepare("SELECT main_stream_url_enc, sub_stream_url_enc, password_enc FROM cameras")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let main: Vec<u8> = row.get(0)?;
        let sub: Option<Vec<u8>> = row.get(1)?;
        let password: Option<Vec<u8>> = row.get(2)?;
        secret_box
            .decrypt(&main)
            .map_err(|_| anyhow::anyhow!("CREDENTIALS_KEY cannot decrypt camera credentials"))?;
        if let Some(value) = sub {
            secret_box.decrypt(&value).map_err(|_| {
                anyhow::anyhow!("CREDENTIALS_KEY cannot decrypt camera credentials")
            })?;
        }
        if let Some(value) = password {
            secret_box.decrypt(&value).map_err(|_| {
                anyhow::anyhow!("CREDENTIALS_KEY cannot decrypt camera credentials")
            })?;
        }
    }
    Ok(())
}

fn database_write_probe(path: &Path) -> anyhow::Result<()> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.execute_batch("PRAGMA foreign_keys=ON; BEGIN IMMEDIATE;")?;
    let probe_result = connection.execute(
        "INSERT INTO audit_logs (id, action, entity_type, details, created_at) \
         VALUES (?, 'doctor_probe', 'system', '{}', ?)",
        (Uuid::new_v4().to_string(), Utc::now().to_rfc3339()),
    );
    let rollback = connection.execute_batch("ROLLBACK");
    probe_result.context("database write probe failed")?;
    rollback.context("roll back database write probe")?;
    Ok(())
}

fn recording_write_probe(root: &Path) -> anyhow::Result<()> {
    require_directory_without_symlinks(root, "recordings directory")?;
    let path = root.join(format!(".sentinel-doctor-{}", Uuid::new_v4()));
    let mut pending = PendingFile::create(&path)?;
    {
        let mut file = OpenOptions::new().write(true).open(&path)?;
        file.write_all(b"sentinel-storage-probe")?;
        file.sync_all()?;
    }
    let content = fs::read(&path)?;
    ensure!(
        content == b"sentinel-storage-probe",
        "recording storage read/write probe failed"
    );
    fs::remove_file(&path)?;
    pending.commit();
    sync_parent(&path)?;
    Ok(())
}

fn copy_database_online(source: &Path, destination: &Path) -> anyhow::Result<()> {
    require_regular_file_without_symlinks(source, "SQLite backup source")?;
    reject_symlink_components(destination)?;
    let mut pending = PendingFile::create(destination)
        .with_context(|| format!("create SQLite output {}", destination.display()))?;
    let source = Connection::open_with_flags(
        source,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .context("open SQLite online-backup source")?;
    source.busy_timeout(Duration::from_secs(5))?;
    let mut destination_connection = Connection::open_with_flags(
        destination,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .context("open SQLite online-backup destination")?;
    destination_connection.busy_timeout(Duration::from_secs(5))?;
    {
        let backup = SqliteBackup::new(&source, &mut destination_connection)
            .context("start SQLite online backup")?;
        backup
            .run_to_completion(64, Duration::from_millis(5), None)
            .context("copy SQLite pages using the online backup API")?;
    }
    destination_connection
        .execute_batch("PRAGMA journal_mode=DELETE;")
        .context("finalize standalone SQLite backup")?;
    drop(destination_connection);
    File::open(destination)?.sync_all()?;
    sync_parent(destination)?;
    pending.commit();
    Ok(())
}

fn copy_regular_file(source: &Path, destination: &Path) -> anyhow::Result<StoredFile> {
    require_regular_file_without_symlinks(source, "backup source file")?;
    reject_symlink_components(destination)?;
    let mut source_file = open_source_no_follow(source)?;
    let before = source_file.metadata()?;
    let mut pending = PendingFile::create(destination)?;
    let destination_file = OpenOptions::new().write(true).open(destination)?;
    let mut reader = BufReader::new(&mut source_file);
    let mut writer = BufWriter::new(destination_file);
    let mut hasher = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        writer.write_all(&buffer[..read])?;
        bytes = bytes
            .checked_add(read as u64)
            .context("backup file size overflow")?;
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    let after = source_file.metadata()?;
    ensure!(
        stable_metadata(&before, &after),
        "backup source file changed while it was copied"
    );
    sync_parent(destination)?;
    pending.commit();
    Ok(StoredFile {
        path: String::new(),
        bytes,
        sha256: hex_digest(hasher.finalize()),
    })
}

fn stored_file(path: &str, filesystem_path: &Path) -> anyhow::Result<StoredFile> {
    let metadata = fs::metadata(filesystem_path)?;
    Ok(StoredFile {
        path: path.to_string(),
        bytes: metadata.len(),
        sha256: sha256_file(filesystem_path)?,
    })
}

fn verify_stored_file(root: &Path, stored: &StoredFile) -> anyhow::Result<()> {
    validate_relative_path(&stored.path)?;
    verify_exact_file(&root.join(&stored.path), stored)
}

fn verify_exact_file(path: &Path, stored: &StoredFile) -> anyhow::Result<()> {
    require_regular_file_without_symlinks(path, "backup file")?;
    let metadata = fs::metadata(path)?;
    ensure!(metadata.len() == stored.bytes, "backup file size mismatch");
    ensure!(
        sha256_file(path)? == stored.sha256,
        "backup file hash mismatch"
    );
    Ok(())
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut reader = BufReader::new(open_source_no_follow(path)?);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_digest(hasher.finalize()))
}

fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    let mut output = String::with_capacity(digest.as_ref().len() * 2);
    for byte in digest.as_ref() {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("write to String");
    }
    output
}

fn copy_recording_tree(
    source_root: &Path,
    destination_root: &Path,
    expected: Option<&[StoredFile]>,
) -> anyhow::Result<Vec<StoredFile>> {
    require_directory_without_symlinks(source_root, "recordings directory")?;
    require_directory_without_symlinks(destination_root, "recordings backup directory")?;
    let expected = expected.map(|files| {
        files
            .iter()
            .map(|file| (file.path.as_str(), file))
            .collect::<BTreeMap<_, _>>()
    });
    let mut files = Vec::new();
    copy_recording_directory(
        source_root,
        source_root,
        destination_root,
        expected.as_ref(),
        &mut files,
    )?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    if let Some(expected) = expected {
        ensure!(
            files.len() == expected.len()
                && files.iter().all(|file| {
                    expected.get(file.path.as_str()).is_some_and(|expected| {
                        expected.bytes == file.bytes && expected.sha256 == file.sha256
                    })
                }),
            "recording archive changed while it was copied"
        );
    }
    Ok(files)
}

fn copy_recording_directory(
    source_root: &Path,
    source_directory: &Path,
    destination_root: &Path,
    expected: Option<&BTreeMap<&str, &StoredFile>>,
    output: &mut Vec<StoredFile>,
) -> anyhow::Result<()> {
    for entry in sorted_directory_entries(source_directory)? {
        let source = entry.path();
        let metadata = fs::symlink_metadata(&source)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "recordings directory must not contain symbolic links"
        );
        let relative = source
            .strip_prefix(source_root)
            .expect("recording beneath source root");
        let portable = portable_relative_path(relative)?;
        let destination = destination_root.join(relative);
        if metadata.is_dir() {
            create_private_directory(&destination)?;
            copy_recording_directory(source_root, &source, destination_root, expected, output)?;
            if fs::read_dir(&destination)?.next().is_none() {
                fs::remove_dir(&destination)?;
            }
        } else if metadata.is_file() {
            if let Some(expected) = expected {
                ensure!(
                    expected.contains_key(portable.as_str()),
                    "recording archive contains an unexpected file"
                );
            }
            let copied = copy_regular_file(&source, &destination)?;
            output.push(StoredFile {
                path: portable,
                ..copied
            });
        } else {
            bail!("recordings directory contains a non-regular file");
        }
    }
    Ok(())
}

fn verify_recording_tree(root: &Path, expected: &[StoredFile]) -> anyhow::Result<()> {
    require_directory_without_symlinks(root, "recordings directory")?;
    let expected_map: BTreeMap<_, _> = expected
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect();
    let mut actual = BTreeSet::new();
    let mut directories = BTreeSet::new();
    collect_recording_files(root, root, &mut actual, &mut directories)?;
    ensure!(
        actual
            == expected_map
                .keys()
                .map(|path| (*path).to_string())
                .collect(),
        "recordings directory has missing or unexpected files"
    );
    ensure!(
        directories == recording_parent_directories(expected),
        "recordings directory has unexpected directory entries"
    );
    for file in expected {
        verify_exact_file(&root.join(&file.path), file)?;
    }
    Ok(())
}

fn collect_recording_files(
    root: &Path,
    directory: &Path,
    output: &mut BTreeSet<String>,
    directories: &mut BTreeSet<String>,
) -> anyhow::Result<()> {
    for entry in sorted_directory_entries(directory)? {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "recordings directory must not contain symbolic links"
        );
        if metadata.is_dir() {
            directories.insert(portable_relative_path(path.strip_prefix(root)?)?);
            collect_recording_files(root, &path, output, directories)?;
        } else if metadata.is_file() {
            output.insert(portable_relative_path(path.strip_prefix(root)?)?);
        } else {
            bail!("recordings directory contains a non-regular file");
        }
    }
    Ok(())
}

fn recording_parent_directories(files: &[StoredFile]) -> BTreeSet<String> {
    let mut directories = BTreeSet::new();
    for file in files {
        let mut parent = Path::new(&file.path).parent();
        while let Some(path) = parent {
            if path.as_os_str().is_empty() {
                break;
            }
            directories.insert(
                portable_relative_path(path).expect("validated manifest recording parent path"),
            );
            parent = path.parent();
        }
    }
    directories
}

fn recording_totals(files: &[StoredFile]) -> anyhow::Result<(u64, u64)> {
    let count = u64::try_from(files.len()).context("too many recording files")?;
    let bytes = files.iter().try_fold(0u64, |total, file| {
        total
            .checked_add(file.bytes)
            .context("recording size overflow")
    })?;
    Ok((count, bytes))
}

fn sorted_directory_entries(directory: &Path) -> anyhow::Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

fn portable_relative_path(path: &Path) -> anyhow::Result<String> {
    validate_relative_path(path)?;
    let parts = path
        .components()
        .map(|component| match component {
            Component::Normal(value) => value.to_str().context("backup paths must be valid UTF-8"),
            _ => unreachable!("validated relative path"),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(parts.join("/"))
}

fn validate_relative_path(path: impl AsRef<Path>) -> anyhow::Result<()> {
    let path = path.as_ref();
    ensure!(
        !path.as_os_str().is_empty(),
        "backup path must not be empty"
    );
    ensure!(!path.is_absolute(), "backup path must be relative");
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "backup path contains a traversal component"
    );
    Ok(())
}

fn validate_source_layout(
    config: &Path,
    contract: &Path,
    binary: &Path,
    recordings: &Path,
) -> anyhow::Result<()> {
    require_regular_file_without_symlinks(config, "MediaMTX config")?;
    require_regular_file_without_symlinks(contract, "MediaMTX contract")?;
    require_regular_file_without_symlinks(binary, "MediaMTX binary")?;
    require_directory_without_symlinks(recordings, "recordings directory")?;
    ensure_distinct_paths(&[config, contract, binary, recordings])
}

fn verify_companion(
    contract_path: &Path,
    binary_path: &Path,
    config_path: &Path,
    recordings_directory: &Path,
) -> anyhow::Result<ParsedContract> {
    validate_source_layout(
        config_path,
        contract_path,
        binary_path,
        recordings_directory,
    )?;
    let contract = verify_binary_contract(contract_path, binary_path)?;
    verify_media_config(config_path, Some(recordings_directory))?;
    Ok(contract)
}

fn verify_binary_contract(
    contract_path: &Path,
    binary_path: &Path,
) -> anyhow::Result<ParsedContract> {
    require_regular_file_without_symlinks(contract_path, "MediaMTX contract")?;
    require_regular_file_without_symlinks(binary_path, "MediaMTX binary")?;
    let contract = parse_contract(contract_path)?;
    ensure!(
        contract.platform == "linux_amd64",
        "MediaMTX companion platform is unsupported"
    );
    ensure!(
        sha256_file(binary_path)? == contract.sha256,
        "MediaMTX binary hash does not match its contract"
    );
    let version_output = Command::new(binary_path)
        .arg("--version")
        .output()
        .context("execute MediaMTX version check")?;
    ensure!(
        version_output.status.success(),
        "MediaMTX version check failed"
    );
    ensure!(
        version_output.stdout.len() <= 256 && version_output.stderr.len() <= 256,
        "MediaMTX version output is unexpectedly large"
    );
    let actual_version =
        String::from_utf8(version_output.stdout).context("MediaMTX version output is not UTF-8")?;
    ensure!(
        actual_version.trim() == contract.version,
        "MediaMTX binary version does not match its contract"
    );
    Ok(contract)
}

fn parse_contract(path: &Path) -> anyhow::Result<ParsedContract> {
    let content = String::from_utf8(read_limited(path, 16 * 1024)?)
        .context("MediaMTX contract is not UTF-8")?;
    let mut values = BTreeMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .context("MediaMTX contract contains an invalid line")?;
        ensure!(
            matches!(key, "version" | "platform" | "sha256"),
            "MediaMTX contract contains an unknown field"
        );
        ensure!(
            values.insert(key, value.trim()).is_none(),
            "MediaMTX contract contains a duplicate field"
        );
    }
    ensure!(values.len() == 3, "MediaMTX contract is incomplete");
    let version = values["version"].to_string();
    let platform = values["platform"].to_string();
    let sha256 = values["sha256"].to_ascii_lowercase();
    ensure!(!version.is_empty(), "MediaMTX version is missing");
    ensure!(
        platform == "linux_amd64",
        "MediaMTX platform is unsupported"
    );
    validate_sha256(&sha256)?;
    Ok(ParsedContract {
        version,
        platform,
        sha256,
    })
}

fn verify_media_config(
    path: &Path,
    expected_recordings_directory: Option<&Path>,
) -> anyhow::Result<()> {
    require_regular_file_without_symlinks(path, "MediaMTX config")?;
    let content = String::from_utf8(read_limited(path, 1024 * 1024)?)
        .context("MediaMTX config is not UTF-8")?;
    for (key, expected) in [
        ("authMethod", "http"),
        (
            "authHTTPAddress",
            "http://127.0.0.1:8080/internal/media/auth",
        ),
        ("apiAddress", "127.0.0.1:9997"),
        ("playbackAddress", "127.0.0.1:9996"),
        ("recordFormat", "fmp4"),
    ] {
        let values = config_values(&content, key);
        ensure!(
            values.len() == 1 && values[0].trim_matches(['\'', '"']) == expected,
            "MediaMTX config has a missing, duplicate or invalid companion setting"
        );
    }
    let record_paths = config_values(&content, "recordPath");
    ensure!(
        record_paths.len() == 1,
        "MediaMTX config must declare exactly one recordPath"
    );
    let record_path = record_paths[0].trim_matches(['\'', '"']);
    let prefix = record_path
        .split_once("%path")
        .map(|(prefix, _)| prefix)
        .context("MediaMTX recordPath must be rooted below a %path component")?
        .trim_end_matches('/');
    ensure!(
        !prefix.is_empty(),
        "MediaMTX recordPath has an empty storage root"
    );
    let configured_root = PathBuf::from(prefix);
    ensure!(
        configured_root.is_absolute(),
        "MediaMTX recording root must be absolute"
    );
    if let Some(expected) = expected_recordings_directory {
        let configured = resolve_existing_or_destination(&configured_root)
            .context("resolve MediaMTX recording root")?;
        let expected =
            resolve_existing_or_destination(expected).context("resolve recordings directory")?;
        ensure!(
            configured == expected,
            "MediaMTX config points at a different recordings directory"
        );
    }
    Ok(())
}

fn config_values<'a>(content: &'a str, key: &str) -> Vec<&'a str> {
    let prefix = format!("{key}:");
    content
        .lines()
        .filter_map(|line| line.trim().strip_prefix(&prefix).map(str::trim))
        .collect()
}

fn validate_sha256(value: &str) -> anyhow::Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "SHA-256 value must be 64 lowercase hexadecimal characters"
    );
    Ok(())
}

fn validate_key_id(value: &str) -> anyhow::Result<()> {
    ensure!(
        (1..=128).contains(&value.len()),
        "credentials key ID must contain 1 to 128 characters"
    );
    ensure!(
        value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'-')
        }),
        "credentials key ID must be a non-secret identifier using letters, digits, . _ : / or -"
    );
    Ok(())
}

fn read_limited(path: &Path, limit: u64) -> anyhow::Result<Vec<u8>> {
    require_regular_file_without_symlinks(path, "input file")?;
    let metadata = fs::metadata(path)?;
    ensure!(metadata.len() <= limit, "input file exceeds its size limit");
    let mut output = Vec::with_capacity(metadata.len() as usize);
    open_source_no_follow(path)?.read_to_end(&mut output)?;
    ensure!(
        output.len() as u64 == metadata.len(),
        "input file changed while it was read"
    );
    Ok(output)
}

fn database_path(database_url: &str) -> anyhow::Result<PathBuf> {
    let value = database_url
        .strip_prefix("sqlite://")
        .or_else(|| database_url.strip_prefix("sqlite:"))
        .context("DATABASE_URL must use the sqlite scheme")?;
    ensure!(!value.is_empty(), "SQLite database path must not be empty");
    ensure!(
        value != ":memory:",
        "in-memory databases cannot be backed up or restored"
    );
    ensure!(
        !value.contains('?')
            && !value.contains('#')
            && !value.contains('%')
            && !value.contains('\0'),
        "operations require a plain, unescaped SQLite file URL"
    );
    Ok(PathBuf::from(value))
}

fn write_manifest(path: &Path, manifest: &BackupManifest) -> anyhow::Result<()> {
    let mut pending = PendingFile::create(path)?;
    let file = OpenOptions::new().write(true).open(path)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, manifest)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    sync_parent(path)?;
    pending.commit();
    Ok(())
}

fn create_private_directory(path: &Path) -> anyhow::Result<()> {
    reject_symlink_components(path)?;
    fs::create_dir(path).with_context(|| format!("create directory {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn require_regular_file_without_symlinks(path: &Path, description: &str) -> anyhow::Result<()> {
    reject_symlink_components(path)?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("{description} does not exist: {}", path.display()))?;
    ensure!(metadata.is_file(), "{description} is not a regular file");
    Ok(())
}

fn require_directory_without_symlinks(path: &Path, description: &str) -> anyhow::Result<()> {
    reject_symlink_components(path)?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("{description} does not exist: {}", path.display()))?;
    ensure!(metadata.is_dir(), "{description} is not a directory");
    Ok(())
}

fn reject_symlink_components(path: &Path) -> anyhow::Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) => ensure!(
                !metadata.file_type().is_symlink(),
                "symbolic links are not accepted in operational paths"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn ensure_distinct_paths(paths: &[&Path]) -> anyhow::Result<()> {
    let mut seen = BTreeSet::new();
    for path in paths {
        let canonical = fs::canonicalize(path)?;
        ensure!(
            seen.insert(canonical),
            "operational paths must refer to distinct filesystem objects"
        );
    }
    Ok(())
}

fn open_source_no_follow(path: &Path) -> anyhow::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    options
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

fn stable_metadata(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.mtime_nsec() == after.mtime_nsec()
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn sync_tree(path: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "cannot sync a symbolic link"
    );
    if metadata.is_dir() {
        for entry in sorted_directory_entries(path)? {
            sync_tree(&entry.path())?;
        }
        File::open(path)?.sync_all()?;
    } else if metadata.is_file() {
        File::open(path)?.sync_all()?;
    } else {
        bail!("cannot sync a non-regular filesystem object");
    }
    Ok(())
}

fn sync_parent(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        File::open(path.parent().unwrap_or_else(|| Path::new(".")))?.sync_all()?;
    }
    Ok(())
}

struct PendingFile {
    path: PathBuf,
    committed: bool,
}

impl PendingFile {
    fn create(path: &Path) -> anyhow::Result<Self> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        options.open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            committed: false,
        })
    }

    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for PendingFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.path);
            let _ = remove_sqlite_sidecars(&self.path);
        }
    }
}

struct PendingDirectory {
    path: PathBuf,
    committed: bool,
}

impl PendingDirectory {
    fn create(path: &Path) -> anyhow::Result<Self> {
        create_private_directory(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            committed: false,
        })
    }

    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for PendingDirectory {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

struct StagedPaths {
    paths: Vec<PathBuf>,
    committed: bool,
}

impl StagedPaths {
    fn new(paths: Vec<PathBuf>) -> Self {
        Self {
            paths,
            committed: false,
        }
    }

    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for StagedPaths {
    fn drop(&mut self) {
        if !self.committed {
            for path in &self.paths {
                let _ = remove_path(path);
                let _ = remove_sqlite_sidecars(path);
            }
        }
    }
}

struct ServiceLocks {
    _database: DatabaseMaintenanceLock,
    _files: Vec<File>,
}

impl ServiceLocks {
    fn acquire(database_url: &str, runtime_directory: &Path) -> anyhow::Result<Self> {
        // Acquire database maintenance before runtime/companion locks everywhere
        // to keep a single global order and fence a same-DB process that was
        // accidentally configured with another runtime directory.
        let database = DatabaseMaintenanceLock::exclusive(database_url)?;
        require_directory_without_symlinks(runtime_directory, "runtime directory")?;
        let mut files = Vec::new();
        for name in ["app.lock", "mediamtx.lock"] {
            let path = runtime_directory.join(name);
            reject_symlink_components(&path)?;
            let mut options = OpenOptions::new();
            options.read(true).write(true).create(true);
            #[cfg(unix)]
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
            let file = options
                .open(&path)
                .with_context(|| format!("open service lock {}", path.display()))?;
            let metadata = file.metadata()?;
            ensure!(metadata.is_file(), "service lock must be a regular file");
            #[cfg(unix)]
            ensure!(
                metadata.nlink() == 1,
                "service lock must not have hard-link aliases"
            );
            lock_file_exclusive(&file).with_context(|| {
                format!(
                    "service lock {} is held; stop Sentinel and MediaMTX first",
                    path.display()
                )
            })?;
            files.push(file);
        }
        for name in ["app.pid", "mediamtx.pid"] {
            ensure_pid_stopped(&runtime_directory.join(name))?;
        }
        Ok(Self {
            _database: database,
            _files: files,
        })
    }
}

fn lock_file_exclusive(file: &File) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        bail!("Sentinel backup service locks are supported only on Unix")
    }
}

fn ensure_pid_stopped(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                !metadata.file_type().is_symlink() && metadata.is_file(),
                "service PID path must be a regular file"
            );
            #[cfg(unix)]
            ensure!(
                metadata.nlink() == 1,
                "service PID path must not have hard-link aliases"
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    let value = String::from_utf8(read_limited(path, 64)?)?;
    let pid = value
        .trim()
        .parse::<u32>()
        .context("service PID file is invalid")?;
    ensure!(pid > 1, "service PID file contains an invalid PID");
    #[cfg(target_os = "linux")]
    ensure!(
        !Path::new("/proc").join(pid.to_string()).exists(),
        "a service PID is still running; stop Sentinel and MediaMTX first"
    );
    Ok(())
}

fn ensure_backup_output_disjoint(
    output: &Path,
    database: &Path,
    config: &Path,
    contract: &Path,
    binary: &Path,
    recordings: &Path,
) -> anyhow::Result<()> {
    let output = resolve_destination(output)?;
    for source in [database, config, contract, binary, recordings] {
        let source = fs::canonicalize(source)?;
        ensure!(
            output != source && !output.starts_with(&source),
            "backup output must be outside every backed-up source"
        );
    }
    Ok(())
}

fn validate_restore_destinations(
    input: &Path,
    database: &Path,
    config: &Path,
    recordings: &Path,
) -> anyhow::Result<()> {
    for path in [database, config, recordings] {
        reject_symlink_components(path)?;
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        require_directory_without_symlinks(parent, "restore destination parent")?;
    }
    if database.exists() {
        require_regular_file_without_symlinks(database, "restore database destination")?;
    }
    if config.exists() {
        require_regular_file_without_symlinks(config, "restore config destination")?;
    }
    if recordings.exists() {
        require_directory_without_symlinks(recordings, "restore recordings destination")?;
    }
    let resolved = [database, config, recordings]
        .into_iter()
        .map(resolve_destination)
        .collect::<Result<Vec<_>, _>>()?;
    for (index, left) in resolved.iter().enumerate() {
        for right in &resolved[index + 1..] {
            ensure!(
                left != right && !left.starts_with(right) && !right.starts_with(left),
                "restore destinations must be distinct and non-overlapping"
            );
        }
    }
    let input = fs::canonicalize(input)?;
    ensure!(
        resolved.iter().all(|destination| {
            input != *destination
                && !input.starts_with(destination)
                && !destination.starts_with(&input)
        }),
        "restore input must be outside every restore destination"
    );
    Ok(())
}

fn resolve_destination(path: &Path) -> anyhow::Result<PathBuf> {
    let parent = fs::canonicalize(path.parent().unwrap_or_else(|| Path::new(".")))?;
    let name = path
        .file_name()
        .context("restore destination must have a file name")?;
    Ok(parent.join(name))
}

fn resolve_existing_or_destination(path: &Path) -> anyhow::Result<PathBuf> {
    if path.exists() {
        fs::canonicalize(path).map_err(Into::into)
    } else {
        resolve_destination(path)
    }
}

fn adjacent_temporary(destination: &Path, label: &str) -> anyhow::Result<PathBuf> {
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    require_directory_without_symlinks(parent, "restore destination parent")?;
    let name = destination
        .file_name()
        .and_then(OsStr::to_str)
        .context("restore destination must have a UTF-8 file name")?;
    for _ in 0..32 {
        let candidate = parent.join(format!(".{name}.{label}-{}.tmp", Uuid::new_v4()));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    bail!("could not allocate an adjacent restore path")
}

fn lock_destination_database(path: &Path) -> anyhow::Result<Option<Connection>> {
    if !path.exists() {
        return Ok(None);
    }
    require_regular_file_without_symlinks(path, "restore database destination")?;
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(Duration::from_secs(2))?;
    let (busy, _, _): (i64, i64, i64) =
        connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    ensure!(busy == 0, "restore database is busy; stop the service");
    connection.pragma_update(None, "locking_mode", "EXCLUSIVE")?;
    connection
        .execute_batch("BEGIN EXCLUSIVE")
        .context("restore database is in use; stop the service")?;
    Ok(Some(connection))
}

struct Replacement {
    destination: PathBuf,
    staged: PathBuf,
    rollback: Option<PathBuf>,
    installed: bool,
}

impl Replacement {
    fn new(destination: PathBuf, staged: PathBuf) -> Self {
        Self {
            destination,
            staged,
            rollback: None,
            installed: false,
        }
    }
}

fn install_replacements(
    replacements: &mut [Replacement],
    fail_after_install: Option<usize>,
) -> anyhow::Result<()> {
    for (index, replacement) in replacements.iter_mut().enumerate() {
        if replacement.destination.exists() {
            let rollback = adjacent_temporary(&replacement.destination, "rollback")?;
            fs::rename(&replacement.destination, &rollback).with_context(|| {
                format!(
                    "preserve {} for restore rollback",
                    replacement.destination.display()
                )
            })?;
            sync_parent(&replacement.destination)?;
            replacement.rollback = Some(rollback);
        }
        fs::rename(&replacement.staged, &replacement.destination).with_context(|| {
            format!(
                "atomically install restored {}",
                replacement.destination.display()
            )
        })?;
        replacement.installed = true;
        sync_parent(&replacement.destination)?;
        if fail_after_install == Some(index + 1) {
            bail!("injected restore installation failure")
        }
    }
    Ok(())
}

fn rollback_replacements(replacements: &mut [Replacement]) -> anyhow::Result<()> {
    let mut errors = Vec::new();
    for replacement in replacements.iter_mut().rev() {
        let mut discarded = None;
        if replacement.installed && replacement.destination.exists() {
            match adjacent_temporary(&replacement.destination, "failed-restore").and_then(|path| {
                fs::rename(&replacement.destination, &path)?;
                Ok(path)
            }) {
                Ok(path) => discarded = Some(path),
                Err(error) => errors.push(error),
            }
        }
        if let Some(rollback) = replacement.rollback.take() {
            if let Err(error) = fs::rename(&rollback, &replacement.destination) {
                errors.push(error.into());
                replacement.rollback = Some(rollback);
            }
        }
        if let Some(discarded) = discarded {
            if let Err(error) = remove_path(&discarded) {
                errors.push(error);
            }
        }
        let _ = sync_parent(&replacement.destination);
        replacement.installed = false;
    }
    ensure!(
        errors.is_empty(),
        "restore rollback encountered filesystem errors; preserved rollback artifacts require operator review"
    );
    Ok(())
}

fn cleanup_replacements(replacements: &mut [Replacement]) -> anyhow::Result<()> {
    for replacement in replacements {
        if let Some(rollback) = replacement.rollback.take() {
            remove_path(&rollback)?;
            remove_sqlite_sidecars(&rollback)?;
            sync_parent(&replacement.destination)?;
        }
    }
    Ok(())
}

fn remove_path(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path)?;
        }
        Ok(_) => fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn remove_sqlite_sidecars(path: &Path) -> anyhow::Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        let sidecar = PathBuf::from(sidecar);
        match fs::remove_file(&sidecar) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    const KEY: [u8; 32] = [0x42; 32];
    const KEY_ID: &str = "vault://sentinel/credentials-key/v1";

    struct TestLayout {
        _temporary: TempDir,
        database: PathBuf,
        runtime: PathBuf,
        recordings: PathBuf,
        config: PathBuf,
        contract: PathBuf,
        binary: PathBuf,
        backup: PathBuf,
    }

    impl TestLayout {
        fn new() -> Self {
            let temporary = TempDir::new().unwrap();
            let root = temporary.path();
            let database = root.join("data/sentinel.sqlite3");
            let runtime = root.join("runtime");
            let recordings = root.join("recordings");
            let config = root.join("config/mediamtx.yml");
            let contract = root.join("contract/mediamtx.lock");
            let binary = root.join("bin/mediamtx");
            let backup = root.join("backup.bundle");
            for directory in [
                database.parent().unwrap(),
                runtime.as_path(),
                recordings.as_path(),
                config.parent().unwrap(),
                contract.parent().unwrap(),
                binary.parent().unwrap(),
            ] {
                fs::create_dir_all(directory).unwrap();
            }
            create_product_database(&database, &KEY);
            fs::create_dir_all(recordings.join("cam-main/day-1")).unwrap();
            fs::write(
                recordings.join("cam-main/day-1/segment.mp4"),
                b"recording-one",
            )
            .unwrap();
            fs::write(recordings.join("cam-main/index.json"), b"[]").unwrap();
            write_fake_binary(&binary, "v1.20.0");
            let binary_hash = sha256_file(&binary).unwrap();
            fs::write(
                &contract,
                format!("version=v1.20.0\nplatform=linux_amd64\nsha256={binary_hash}\n"),
            )
            .unwrap();
            write_media_config(&config, &recordings, "");
            Self {
                _temporary: temporary,
                database,
                runtime,
                recordings,
                config,
                contract,
                binary,
                backup,
            }
        }

        fn create_options(&self) -> CreateOptions {
            CreateOptions {
                database_url: format!("sqlite://{}", self.database.display()),
                output: self.backup.clone(),
                mediamtx_config: self.config.clone(),
                mediamtx_contract: self.contract.clone(),
                mediamtx_binary: self.binary.clone(),
                recordings_directory: self.recordings.clone(),
                runtime_directory: self.runtime.clone(),
                credentials_key_id: KEY_ID.to_string(),
                credentials_key: KEY,
            }
        }

        fn restore_options(&self) -> RestoreOptions {
            RestoreOptions {
                database_url: format!("sqlite://{}", self.database.display()),
                input: self.backup.clone(),
                mediamtx_config: self.config.clone(),
                mediamtx_contract: self.contract.clone(),
                mediamtx_binary: self.binary.clone(),
                recordings_directory: self.recordings.clone(),
                runtime_directory: self.runtime.clone(),
                credentials_key_id: KEY_ID.to_string(),
                credentials_key: KEY,
            }
        }
    }

    fn create_product_database(path: &Path, key: &[u8; 32]) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;")
            .unwrap();
        connection
            .execute_batch(include_str!("../migrations/0001_init.sql"))
            .unwrap();
        connection
            .execute_batch(include_str!("../migrations/0002_browser_sessions.sql"))
            .unwrap();
        connection
            .execute_batch(include_str!("../migrations/0003_media_reconciliation.sql"))
            .unwrap();
        connection
            .execute_batch(
                "CREATE TABLE _sqlx_migrations (\
                     version BIGINT PRIMARY KEY,\
                     description TEXT NOT NULL,\
                     installed_on TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,\
                     success BOOLEAN NOT NULL,\
                     checksum BLOB NOT NULL,\
                     execution_time BIGINT NOT NULL\
                 );\
                 INSERT INTO _sqlx_migrations \
                     (version, description, success, checksum, execution_time) VALUES\
                     (1, 'init', 1, X'01', 1),\
                     (2, 'browser sessions', 1, X'02', 1),\
                     (3, 'media reconciliation', 1, X'03', 1);",
            )
            .unwrap();
        let now = Utc::now().to_rfc3339();
        let user = Uuid::new_v4().to_string();
        let camera = Uuid::new_v4().to_string();
        connection
            .execute(
                "INSERT INTO users (id, email, password_hash, role, created_at, updated_at) \
                 VALUES (?, 'admin@example.test', 'hash', 'admin', ?, ?)",
                (&user, &now, &now),
            )
            .unwrap();
        let encrypted = SecretBox::new(key)
            .encrypt("rtsp://camera.local/main")
            .unwrap();
        connection
            .execute(
                "INSERT INTO cameras (id, name, main_stream_url_enc, created_by, created_at, updated_at) \
                 VALUES (?, 'Front', ?, ?, ?, ?)",
                rusqlite::params![camera, encrypted, user, now, now],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO media_desired_states \
                 (camera_id, generation, desired_present, main_path, record_enabled, updated_at) \
                 VALUES (?, 1, 1, 'cam_main', 1, ?)",
                (&camera, &now),
            )
            .unwrap();
    }

    fn write_fake_binary(path: &Path, version: &str) {
        fs::write(path, format!("#!/bin/sh\nprintf '%s\\n' '{version}'\n")).unwrap();
        #[cfg(unix)]
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn write_media_config(path: &Path, recordings: &Path, suffix: &str) {
        fs::write(
            path,
            format!(
                "authMethod: http\n\
                 authHTTPAddress: http://127.0.0.1:8080/internal/media/auth\n\
                 apiAddress: 127.0.0.1:9997\n\
                 playbackAddress: 127.0.0.1:9996\n\
                 recordFormat: fmp4\n\
                 recordPath: {}/%path/%Y-%m-%d_%H-%M-%S-%f\n\
                 {suffix}\n",
                recordings.display()
            ),
        )
        .unwrap();
    }

    fn insert_audit(path: &Path, action: &str) {
        Connection::open(path)
            .unwrap()
            .execute(
                "INSERT INTO audit_logs (id, action, entity_type, created_at) \
                 VALUES (?, ?, 'test', ?)",
                (Uuid::new_v4().to_string(), action, Utc::now().to_rfc3339()),
            )
            .unwrap();
    }

    fn audit_count(path: &Path) -> i64 {
        Connection::open(path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM audit_logs", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn online_backup_captures_a_consistent_wal_snapshot_during_writes() {
        let temporary = TempDir::new().unwrap();
        let source = temporary.path().join("source.sqlite3");
        let destination = temporary.path().join("snapshot.sqlite3");
        create_product_database(&source, &KEY);
        let connection = Connection::open(&source).unwrap();
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;\
                 CREATE TABLE snapshot_invariant (id INTEGER PRIMARY KEY, left_value INTEGER NOT NULL, right_value INTEGER NOT NULL);\
                 INSERT INTO snapshot_invariant VALUES (1, 0, 0);\
                 CREATE TABLE padding (id INTEGER PRIMARY KEY, value BLOB NOT NULL);",
            )
            .unwrap();
        let payload = vec![0x5au8; 8 * 1024];
        for id in 0..1_000 {
            connection
                .execute(
                    "INSERT INTO padding (id, value) VALUES (?, ?)",
                    rusqlite::params![id, &payload],
                )
                .unwrap();
        }
        drop(connection);

        let started = Arc::new(AtomicBool::new(false));
        let writer_started = started.clone();
        let writer_path = source.clone();
        let writer = std::thread::spawn(move || {
            let mut connection = Connection::open(writer_path).unwrap();
            connection.busy_timeout(Duration::from_secs(5)).unwrap();
            writer_started.store(true, Ordering::Release);
            for value in 1..=500i64 {
                let transaction = connection.transaction().unwrap();
                transaction
                    .execute(
                        "UPDATE snapshot_invariant SET left_value = ? WHERE id = 1",
                        [value],
                    )
                    .unwrap();
                transaction
                    .execute(
                        "UPDATE snapshot_invariant SET right_value = ? WHERE id = 1",
                        [value],
                    )
                    .unwrap();
                transaction.commit().unwrap();
                std::thread::sleep(Duration::from_micros(100));
            }
        });
        while !started.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        copy_database_online(&source, &destination).unwrap();
        writer.join().unwrap();

        let snapshot = Connection::open(&destination).unwrap();
        let (left, right): (i64, i64) = snapshot
            .query_row(
                "SELECT left_value, right_value FROM snapshot_invariant WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(left, right);
        assert_eq!(
            snapshot
                .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        assert!(destination.exists());
        assert!(!PathBuf::from(format!("{}-wal", destination.display())).exists());
    }

    #[test]
    fn complete_bundle_is_non_overwriting_verified_and_restored() {
        let layout = TestLayout::new();
        let manifest = create(&layout.create_options()).unwrap();
        assert_eq!(manifest.data_files, 2);
        assert_eq!(manifest.database_schema, 3);
        assert!(!manifest.credentials_key.included);
        verify(&layout.backup).unwrap();
        assert!(create(&layout.create_options()).is_err());

        #[cfg(unix)]
        {
            assert_eq!(
                fs::metadata(layout.backup.join(MANIFEST_FILE))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&layout.backup).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }

        let manifest_text = fs::read_to_string(layout.backup.join(MANIFEST_FILE)).unwrap();
        assert!(!manifest_text.contains(&STANDARD.encode(KEY)));
        insert_audit(&layout.database, "after-backup");
        write_media_config(&layout.config, &layout.recordings, "# old config marker");
        fs::write(
            layout.recordings.join("cam-main/day-1/segment.mp4"),
            b"changed recording",
        )
        .unwrap();
        fs::write(layout.recordings.join("extra.mp4"), b"extra").unwrap();

        restore(&layout.restore_options()).unwrap();
        assert_eq!(audit_count(&layout.database), 0);
        assert_eq!(
            fs::read(layout.recordings.join("cam-main/day-1/segment.mp4")).unwrap(),
            b"recording-one"
        );
        assert!(!layout.recordings.join("extra.mp4").exists());
        assert!(!fs::read_to_string(&layout.config)
            .unwrap()
            .contains("old config marker"));
        verify_database(&layout.database).unwrap();
    }

    #[test]
    fn verification_rejects_corruption_wrong_product_missing_files_and_foreign_keys() {
        let layout = TestLayout::new();
        create(&layout.create_options()).unwrap();
        let manifest_path = layout.backup.join(MANIFEST_FILE);
        let original_manifest = fs::read(&manifest_path).unwrap();
        let recording = layout
            .backup
            .join(RECORDINGS_DIRECTORY)
            .join("cam-main/day-1/segment.mp4");
        let original_recording = fs::read(&recording).unwrap();

        fs::write(&recording, b"corrupt").unwrap();
        assert!(verify(&layout.backup).is_err());
        fs::write(&recording, &original_recording).unwrap();

        let mut manifest: BackupManifest = serde_json::from_slice(&original_manifest).unwrap();
        manifest.application = "photo-backup".to_string();
        write_manifest_replace(&manifest_path, &manifest);
        assert!(verify(&layout.backup).is_err());
        fs::write(&manifest_path, &original_manifest).unwrap();

        fs::remove_file(&recording).unwrap();
        assert!(verify(&layout.backup).is_err());
        fs::write(&recording, &original_recording).unwrap();
        fs::write(&manifest_path, &original_manifest).unwrap();

        let invalid_database = layout._temporary.path().join("invalid-foreign-key.sqlite3");
        create_product_database(&invalid_database, &KEY);
        let connection = Connection::open(&invalid_database).unwrap();
        connection
            .execute_batch("PRAGMA foreign_keys=OFF;")
            .unwrap();
        connection
            .execute(
                "INSERT INTO events (id, camera_id, kind, severity, message, created_at) \
                 VALUES (?, ?, 'test', 'info', 'broken', ?)",
                (
                    Uuid::new_v4().to_string(),
                    Uuid::new_v4().to_string(),
                    Utc::now().to_rfc3339(),
                ),
            )
            .unwrap();
        drop(connection);
        assert!(verify_database(&invalid_database).is_err());

        let corrupt_database = layout._temporary.path().join("corrupt.sqlite3");
        fs::write(&corrupt_database, b"not a sqlite database").unwrap();
        assert!(verify_database(&corrupt_database).is_err());
        let wrong_database = layout._temporary.path().join("wrong.sqlite3");
        Connection::open(&wrong_database)
            .unwrap()
            .execute("CREATE TABLE unrelated(id INTEGER)", [])
            .unwrap();
        assert!(verify_database(&wrong_database).is_err());
    }

    #[test]
    fn partial_restore_failure_rolls_back_database_config_and_recordings() {
        let layout = TestLayout::new();
        create(&layout.create_options()).unwrap();
        insert_audit(&layout.database, "must-survive-failed-restore");
        write_media_config(
            &layout.config,
            &layout.recordings,
            "# preserved config marker",
        );
        fs::write(
            layout.recordings.join("cam-main/day-1/segment.mp4"),
            b"preserved recording",
        )
        .unwrap();

        assert!(restore_inner(&layout.restore_options(), Some(1)).is_err());
        assert_eq!(audit_count(&layout.database), 1);
        assert!(fs::read_to_string(&layout.config)
            .unwrap()
            .contains("preserved config marker"));
        assert_eq!(
            fs::read(layout.recordings.join("cam-main/day-1/segment.mp4")).unwrap(),
            b"preserved recording"
        );
        verify_database(&layout.database).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_and_manifest_path_traversal_are_rejected() {
        use std::os::unix::fs::symlink;

        let layout = TestLayout::new();
        let outside = layout._temporary.path().join("outside.mp4");
        fs::write(&outside, b"outside").unwrap();
        symlink(&outside, layout.recordings.join("linked.mp4")).unwrap();
        assert!(create(&layout.create_options()).is_err());
        fs::remove_file(layout.recordings.join("linked.mp4")).unwrap();

        let mut nested_output = layout.create_options();
        nested_output.output = layout.recordings.join("recursive-backup");
        assert!(create(&nested_output).is_err());
        assert!(!nested_output.output.exists());
        create(&layout.create_options()).unwrap();

        let manifest_path = layout.backup.join(MANIFEST_FILE);
        let mut manifest: BackupManifest =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest.recordings.files[0].path = "../escape.mp4".to_string();
        write_manifest_replace(&manifest_path, &manifest);
        assert!(verify(&layout.backup).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn held_service_lock_refuses_backup() {
        let layout = TestLayout::new();
        let database_url = format!("sqlite://{}", layout.database.display());
        let _lock =
            crate::runtime_lock::ApplicationLock::acquire(&database_url, &layout.runtime).unwrap();
        assert!(create(&layout.create_options()).is_err());
        assert!(!layout.backup.exists());
    }

    #[tokio::test]
    async fn offline_doctor_checks_write_storage_companion_and_credentials() {
        let layout = TestLayout::new();
        let before = audit_count(&layout.database);
        let report = doctor(&DoctorOptions {
            database_url: format!("sqlite://{}", layout.database.display()),
            mediamtx_config: layout.config.clone(),
            mediamtx_contract: layout.contract.clone(),
            mediamtx_binary: layout.binary.clone(),
            recordings_directory: layout.recordings.clone(),
            credentials_key: KEY,
            app_ready_url: "http://127.0.0.1:1/health/ready".to_string(),
            mediamtx_ready_url: "http://127.0.0.1:1/v3/info".to_string(),
            offline: true,
        })
        .await
        .unwrap();
        assert_eq!(report.status, "ok");
        assert_eq!(audit_count(&layout.database), before);
        assert!(!layout.recordings.read_dir().unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".sentinel-doctor-")));

        let wrong_options = DoctorOptions {
            database_url: format!("sqlite://{}", layout.database.display()),
            mediamtx_config: layout.config,
            mediamtx_contract: layout.contract,
            mediamtx_binary: layout.binary,
            recordings_directory: layout.recordings,
            credentials_key: [0x99; 32],
            app_ready_url: String::new(),
            mediamtx_ready_url: String::new(),
            offline: true,
        };
        assert!(doctor(&wrong_options).await.is_err());
        assert!(live_probe("http://example.com/health/ready").await.is_err());
    }

    fn write_manifest_replace(path: &Path, manifest: &BackupManifest) {
        let file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(path)
            .unwrap();
        serde_json::to_writer_pretty(file, manifest).unwrap();
    }
}
