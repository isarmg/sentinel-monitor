use anyhow::{ensure, Context};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{params, Connection, OpenFlags};
use sha2::{Digest, Sha256};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
    SqlitePool,
};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    str::FromStr,
    time::Duration,
};
use uuid::{Uuid, Version};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

const MAX_CONNECTIONS: u32 = 10;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const APPLICATION: &str = "sentinel-monitor";
pub const CURRENT_SCHEMA_REVISION: i64 = 1;
pub const CURRENT_SCHEMA_SHA256: &str =
    "f547ddc817d830d23b5305bb1f88b29898d6531568edd6eb194c2b629eb560c0";
const CURRENT_SCHEMA: &str = include_str!("current_schema.sql");
const PRODUCT_METADATA_SQL: &str = "CREATE TABLE product_metadata (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
    application TEXT NOT NULL,
    application_version TEXT NOT NULL,
    schema_revision INTEGER NOT NULL,
    schema_sha256 TEXT NOT NULL
)";
const GLOBAL_LEASE_SQL: &str = "CREATE TABLE media_reconciler_leases (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
    lease_owner TEXT,
    lease_expires_at TEXT,
    updated_at TEXT NOT NULL,
    CHECK ((lease_owner IS NULL) = (lease_expires_at IS NULL)),
    CHECK (julianday(updated_at) IS NOT NULL),
    CHECK (
        lease_owner IS NULL OR (
            length(lease_owner) = 36
            AND lease_owner = lower(lease_owner)
            AND substr(lease_owner, 9, 1) = '-'
            AND substr(lease_owner, 14, 1) = '-'
            AND substr(lease_owner, 15, 1) = '4'
            AND substr(lease_owner, 19, 1) = '-'
            AND substr(lease_owner, 20, 1) GLOB '[89ab]'
            AND substr(lease_owner, 24, 1) = '-'
            AND lease_owner NOT GLOB '*[^0-9a-f-]*'
            AND length(replace(lease_owner, '-', '')) = 32
        )
    ),
    CHECK (
        lease_expires_at IS NULL OR (
            julianday(lease_expires_at) IS NOT NULL
            AND julianday(lease_expires_at) > julianday(updated_at)
        )
    )
)";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GlobalLeaseState {
    pub owner: Option<String>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

pub async fn open_pool(database_url: &str) -> anyhow::Result<SqlitePool> {
    prepare_current_database(database_url)?;
    let options = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(false)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(BUSY_TIMEOUT)
        .synchronous(SqliteSynchronous::Full);

    let pool = SqlitePoolOptions::new()
        .max_connections(MAX_CONNECTIONS)
        .connect_with(options)
        .await?;
    // Validate again after opening the production pool so a path replacement
    // cannot silently change the current-schema contract.
    validate_current_database(&database_path(database_url)?)?;
    Ok(pool)
}

pub(crate) fn prepare_current_database(database_url: &str) -> anyhow::Result<()> {
    let path = database_path(database_url)?;
    require_real_parent(&path)?;
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            initialize_current_database(&path)
        }
        Err(error) => Err(error.into()),
        Ok(_) => validate_current_database(&path),
    }
}

pub(crate) fn validate_current_database(path: &Path) -> anyhow::Result<()> {
    require_secure_database_file(path)?;
    let snapshot = snapshot_generation(path)?;
    let connection = Connection::open_with_flags(
        &snapshot.database,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .context("open private SQLite validation snapshot")?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    validate_current_connection(&connection)
}

struct ValidationSnapshot {
    _directory: tempfile::TempDir,
    database: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SourceSnapshot {
    hash: [u8; 32],
    length: u64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

fn snapshot_generation(path: &Path) -> anyhow::Result<ValidationSnapshot> {
    let mut last_change = None;
    for _ in 0..4 {
        match snapshot_generation_once(path) {
            Ok(snapshot) => return Ok(snapshot),
            Err(error) if error.is::<GenerationChanged>() => {
                last_change = Some(error);
                std::thread::yield_now();
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_change.expect("snapshot retry loop records every generation change"))
}

#[derive(Debug, thiserror::Error)]
#[error("SQLite generation changed during current-schema validation")]
struct GenerationChanged;

fn snapshot_generation_once(path: &Path) -> anyhow::Result<ValidationSnapshot> {
    let directory = tempfile::Builder::new()
        .prefix("sentinel-schema-check-")
        .tempdir()
        .context("create private current-schema validation directory")?;
    let database = directory.path().join("database.sqlite3");
    let sources = [
        path.to_path_buf(),
        sqlite_sidecar(path, "-wal"),
        sqlite_sidecar(path, "-journal"),
    ];
    let destinations = [
        database.clone(),
        sqlite_sidecar(&database, "-wal"),
        sqlite_sidecar(&database, "-journal"),
    ];

    // SQLite read-only WAL connections still write lock bytes to `-shm`.
    // Validate a private generation copy instead so every rejection leaves the
    // source main/WAL/journal/SHM bytes exactly as they were.
    let mut expected = Vec::with_capacity(sources.len());
    for (source, destination) in sources.iter().zip(&destinations) {
        expected.push(copy_generation_file(source, destination)?);
    }
    let _ = source_snapshot(&sqlite_sidecar(path, "-shm"))?;

    // Detect a writer, checkpoint or path replacement racing the copy. The
    // database instance lock makes this stable for a correctly configured
    // product; an out-of-protocol writer is rejected rather than snapshotted.
    for (source, expected) in sources.iter().zip(expected) {
        if source_snapshot(source)? != expected {
            return Err(GenerationChanged.into());
        }
    }

    Ok(ValidationSnapshot {
        _directory: directory,
        database,
    })
}

fn copy_generation_file(
    source_path: &Path,
    destination_path: &Path,
) -> anyhow::Result<Option<SourceSnapshot>> {
    let Some((mut source, before)) = open_source_snapshot(source_path)? else {
        return Ok(None);
    };
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let mut destination = options.open(destination_path)?;
    std::io::copy(&mut source, &mut destination)?;
    destination.flush()?;
    destination.sync_all()?;
    destination.seek(SeekFrom::Start(0))?;
    let copied_hash = hash_reader(&mut destination)?;
    let Some(after) = source_snapshot(source_path)? else {
        return Err(GenerationChanged.into());
    };
    if before != after || copied_hash != after.hash {
        return Err(GenerationChanged.into());
    }
    Ok(Some(after))
}

fn source_snapshot(path: &Path) -> anyhow::Result<Option<SourceSnapshot>> {
    let Some((_, snapshot)) = open_source_snapshot(path)? else {
        return Ok(None);
    };
    Ok(Some(snapshot))
}

fn open_source_snapshot(path: &Path) -> anyhow::Result<Option<(File, SourceSnapshot)>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let opened = file.metadata()?;
    let named = fs::symlink_metadata(path)?;
    ensure!(
        opened.is_file() && named.is_file() && !named.file_type().is_symlink(),
        "SQLite generation must contain only regular files without symbolic links"
    );
    #[cfg(unix)]
    ensure!(
        opened.nlink() == 1
            && named.nlink() == 1
            && opened.dev() == named.dev()
            && opened.ino() == named.ino(),
        "SQLite generation files must not have hard-link aliases or change while opened"
    );
    let hash = hash_reader(&mut file)?;
    file.seek(SeekFrom::Start(0))?;
    Ok(Some((
        file,
        SourceSnapshot {
            hash,
            length: opened.len(),
            #[cfg(unix)]
            device: opened.dev(),
            #[cfg(unix)]
            inode: opened.ino(),
        },
    )))
}

fn hash_reader(reader: &mut impl Read) -> anyhow::Result<[u8; 32]> {
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

pub(crate) fn integrity_and_foreign_key_check(path: &Path) -> anyhow::Result<()> {
    require_secure_database_file(path)?;
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    ensure!(
        integrity.eq_ignore_ascii_case("ok"),
        "SQLite integrity check failed"
    );
    let mut foreign_keys = connection.prepare("PRAGMA foreign_key_check")?;
    ensure!(
        foreign_keys.query([])?.next()?.is_none(),
        "SQLite foreign-key check failed"
    );
    Ok(())
}

pub(crate) fn database_path(database_url: &str) -> anyhow::Result<PathBuf> {
    let value = database_url
        .strip_prefix("sqlite://")
        .or_else(|| database_url.strip_prefix("sqlite:"))
        .context("DATABASE_URL must use the sqlite scheme")?;
    ensure!(!value.is_empty(), "SQLite database path must not be empty");
    ensure!(value != ":memory:", "in-memory SQLite is not supported");
    ensure!(
        !value.contains('?')
            && !value.contains('#')
            && !value.contains('%')
            && !value.contains('\0'),
        "DATABASE_URL must be a plain, unescaped SQLite file URL"
    );
    let path = PathBuf::from(value);
    ensure!(path.is_absolute(), "SQLite database path must be absolute");
    ensure!(
        path.file_name().is_some(),
        "SQLite database path must name a file"
    );
    ensure!(
        !path
            .components()
            .any(|component| matches!(component, Component::ParentDir)),
        "SQLite database path must not contain parent traversal"
    );
    Ok(path)
}

fn initialize_current_database(path: &Path) -> anyhow::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let reserved = options
        .open(path)
        .with_context(|| format!("create current SQLite database {}", path.display()))?;
    reserved.sync_all()?;
    drop(reserved);

    let result = (|| {
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        connection.execute_batch("PRAGMA foreign_keys=ON; BEGIN IMMEDIATE;")?;
        let transaction_result = (|| {
            connection.execute_batch(CURRENT_SCHEMA)?;
            let actual = schema_fingerprint(&connection)?;
            ensure!(
                actual == CURRENT_SCHEMA_SHA256,
                "compiled current schema fingerprint mismatch: {actual}"
            );
            connection.execute(
                "INSERT INTO product_metadata (
                     singleton, application, application_version, schema_revision, schema_sha256
                 ) VALUES (1, ?, ?, ?, ?)",
                params![
                    APPLICATION,
                    env!("CARGO_PKG_VERSION"),
                    CURRENT_SCHEMA_REVISION,
                    CURRENT_SCHEMA_SHA256
                ],
            )?;
            validate_current_connection(&connection)
        })();
        match transaction_result {
            Ok(()) => connection.execute_batch("COMMIT")?,
            Err(error) => {
                let _ = connection.execute_batch("ROLLBACK");
                return Err(error);
            }
        }
        drop(connection);
        File::open(path)?.sync_all()?;
        sync_parent(path)?;
        Ok(())
    })();

    if result.is_err() {
        for candidate in sqlite_generation_paths(path) {
            let _ = fs::remove_file(candidate);
        }
        let _ = sync_parent(path);
    }
    result
}

fn validate_current_connection(connection: &Connection) -> anyhow::Result<()> {
    validate_product_metadata_table(connection)?;
    let count: i64 = connection.query_row("SELECT COUNT(*) FROM product_metadata", [], |row| {
        row.get(0)
    })?;
    ensure!(count == 1, "product_metadata must contain exactly one row");
    let (singleton, application, version, revision, expected_fingerprint): (
        i64,
        String,
        String,
        i64,
        String,
    ) = connection.query_row(
        "SELECT singleton, application, application_version, schema_revision, schema_sha256
         FROM product_metadata",
        [],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )?;
    ensure!(singleton == 1, "product_metadata singleton is invalid");
    ensure!(
        application == APPLICATION,
        "database belongs to a different application"
    );
    ensure!(
        version == env!("CARGO_PKG_VERSION"),
        "database application version is not exactly current"
    );
    ensure!(
        revision == CURRENT_SCHEMA_REVISION,
        "database schema revision is not exactly current"
    );
    ensure!(
        expected_fingerprint == CURRENT_SCHEMA_SHA256,
        "database schema fingerprint metadata is not exactly current"
    );
    validate_global_lease_table(connection)?;
    let actual = schema_fingerprint(connection)?;
    ensure!(
        actual == CURRENT_SCHEMA_SHA256,
        "actual SQLite schema does not match the compiled current schema"
    );
    Ok(())
}

fn validate_product_metadata_table(connection: &Connection) -> anyhow::Result<()> {
    let sql: String = connection
        .query_row(
            "SELECT sql FROM sqlite_schema
             WHERE type = 'table' AND name = 'product_metadata'",
            [],
            |row| row.get(0),
        )
        .context("database has no current product_metadata table")?;
    ensure!(
        normalize_sql(&sql) == normalize_sql(PRODUCT_METADATA_SQL),
        "product_metadata table does not match the current contract"
    );
    let mut columns = connection.prepare("PRAGMA table_info('product_metadata')")?;
    let actual = columns
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let expected = vec![
        ("singleton".to_string(), "INTEGER".to_string(), 1, 1),
        ("application".to_string(), "TEXT".to_string(), 1, 0),
        ("application_version".to_string(), "TEXT".to_string(), 1, 0),
        ("schema_revision".to_string(), "INTEGER".to_string(), 1, 0),
        ("schema_sha256".to_string(), "TEXT".to_string(), 1, 0),
    ];
    ensure!(
        actual == expected,
        "product_metadata columns do not match the current contract"
    );
    Ok(())
}

fn validate_global_lease_table(connection: &Connection) -> anyhow::Result<GlobalLeaseState> {
    let sql: String = connection
        .query_row(
            "SELECT sql FROM sqlite_schema
             WHERE type = 'table' AND name = 'media_reconciler_leases'",
            [],
            |row| row.get(0),
        )
        .context("database has no current media_reconciler_leases table")?;
    validate_global_lease_schema_sql(&sql)?;

    let mut columns = connection.prepare("PRAGMA table_info('media_reconciler_leases')")?;
    let actual = columns
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let expected = vec![
        ("singleton".to_string(), "INTEGER".to_string(), 1, 1),
        ("lease_owner".to_string(), "TEXT".to_string(), 0, 0),
        ("lease_expires_at".to_string(), "TEXT".to_string(), 0, 0),
        ("updated_at".to_string(), "TEXT".to_string(), 1, 0),
    ];
    ensure!(
        actual == expected,
        "media_reconciler_leases columns do not match the current contract"
    );

    let mut statement = connection.prepare(
        "SELECT typeof(singleton), singleton, typeof(lease_owner), lease_owner,
                typeof(lease_expires_at), lease_expires_at, typeof(updated_at), updated_at
         FROM media_reconciler_leases ORDER BY singleton",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    ensure!(
        rows.len() == 1,
        "media_reconciler_leases must contain exactly one current row"
    );
    let (
        singleton_storage,
        singleton,
        owner_storage,
        owner,
        expiry_storage,
        expiry,
        updated_storage,
        updated,
    ) = rows.into_iter().next().expect("one lease row was required");
    validate_global_lease_values(
        &singleton_storage,
        singleton,
        &owner_storage,
        owner.as_deref(),
        &expiry_storage,
        expiry.as_deref(),
        &updated_storage,
        &updated,
    )
}

pub(crate) fn validate_global_lease_schema_sql(sql: &str) -> anyhow::Result<()> {
    ensure!(
        normalize_sql(sql) == normalize_sql(GLOBAL_LEASE_SQL),
        "media_reconciler_leases table does not match the current contract"
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn validate_global_lease_values(
    singleton_storage: &str,
    singleton: i64,
    owner_storage: &str,
    owner: Option<&str>,
    expiry_storage: &str,
    expiry: Option<&str>,
    updated_storage: &str,
    updated: &str,
) -> anyhow::Result<GlobalLeaseState> {
    ensure!(
        singleton_storage == "integer" && singleton == 1,
        "global lease singleton is not exactly current"
    );
    ensure!(
        updated_storage == "text",
        "global lease updated_at storage is not exactly current"
    );
    let updated_at = parse_canonical_utc(updated, "global lease updated_at")?;

    let (owner, lease_expires_at) = match (owner, expiry) {
        (None, None) => {
            ensure!(
                owner_storage == "null" && expiry_storage == "null",
                "free global lease storage is not exactly current"
            );
            (None, None)
        }
        (Some(owner), Some(expiry)) => {
            ensure!(
                owner_storage == "text" && expiry_storage == "text",
                "owned global lease storage is not exactly current"
            );
            let owner_id = Uuid::parse_str(owner).context("global lease owner is not a UUID")?;
            ensure!(
                owner_id.hyphenated().to_string() == owner
                    && owner_id.get_version() == Some(Version::Random),
                "global lease owner is not a canonical lowercase UUIDv4"
            );
            let lease_expires_at = parse_canonical_utc(expiry, "global lease lease_expires_at")?;
            ensure!(
                lease_expires_at > updated_at,
                "global lease expiry must be later than updated_at"
            );
            (Some(owner.to_string()), Some(lease_expires_at))
        }
        _ => anyhow::bail!("global lease owner and expiry must be present or absent together"),
    };

    Ok(GlobalLeaseState {
        owner,
        lease_expires_at,
        updated_at,
    })
}

fn parse_canonical_utc(value: &str, label: &str) -> anyhow::Result<DateTime<Utc>> {
    let parsed =
        DateTime::parse_from_rfc3339(value).with_context(|| format!("{label} is not RFC 3339"))?;
    ensure!(parsed.offset().local_minus_utc() == 0, "{label} is not UTC");
    let parsed = parsed.with_timezone(&Utc);
    ensure!(
        parsed.to_rfc3339_opts(SecondsFormat::AutoSi, false) == value,
        "{label} is not canonical"
    );
    Ok(parsed)
}

fn schema_fingerprint(connection: &Connection) -> anyhow::Result<String> {
    let mut statement = connection.prepare(
        "SELECT type, name, tbl_name, COALESCE(sql, '')
         FROM sqlite_schema
         WHERE name NOT GLOB 'sqlite_*' AND name <> 'product_metadata'
         ORDER BY type, name, tbl_name",
    )?;
    let rows = statement.query_map([], |row| {
        Ok([
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ])
    })?;
    let mut hasher = Sha256::new();
    for row in rows {
        for field in row? {
            let bytes = field.as_bytes();
            let length = u64::try_from(bytes.len()).context("schema field is too large")?;
            hasher.update(length.to_be_bytes());
            hasher.update(bytes);
        }
    }
    Ok(lower_hex(hasher.finalize()))
}

fn normalize_sql(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

fn lower_hex(bytes: impl AsRef<[u8]>) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(bytes.as_ref().len() * 2);
    for byte in bytes.as_ref() {
        write!(&mut output, "{byte:02x}").expect("write to String");
    }
    output
}

fn require_secure_database_file(path: &Path) -> anyhow::Result<()> {
    require_real_parent(path)?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("SQLite database does not exist: {}", path.display()))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "SQLite database must be a regular file without symbolic links"
    );
    #[cfg(unix)]
    ensure!(
        metadata.nlink() == 1,
        "SQLite database must not have hard-link aliases"
    );
    Ok(())
}

fn require_real_parent(path: &Path) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .context("SQLite database must have a parent")?;
    let mut current = PathBuf::new();
    for component in parent.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => current.push("."),
            Component::Normal(value) => current.push(value),
            Component::ParentDir => anyhow::bail!("SQLite path must not contain parent traversal"),
        }
        let metadata = fs::symlink_metadata(&current)
            .with_context(|| format!("SQLite parent does not exist: {}", current.display()))?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "SQLite path must not traverse symbolic links or special files"
        );
    }
    Ok(())
}

fn sqlite_generation_paths(path: &Path) -> [PathBuf; 4] {
    [
        path.to_path_buf(),
        sqlite_sidecar(path, "-wal"),
        sqlite_sidecar(path, "-shm"),
        sqlite_sidecar(path, "-journal"),
    ]
}

fn sqlite_sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn sync_parent(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    File::open(path.parent().unwrap_or_else(|| Path::new(".")))?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn current_schema_fingerprint_matches_the_compiled_contract() {
        let temporary = tempfile::tempdir().unwrap();
        let database = temporary.path().join("current.sqlite3");
        initialize_current_database(&database).unwrap();
        validate_current_database(&database).unwrap();

        let connection = Connection::open(&database).unwrap();
        let metadata: (String, String, i64, String) = connection
            .query_row(
                "SELECT application, application_version, schema_revision, schema_sha256
                 FROM product_metadata WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            metadata,
            (
                APPLICATION.to_string(),
                env!("CARGO_PKG_VERSION").to_string(),
                CURRENT_SCHEMA_REVISION,
                CURRENT_SCHEMA_SHA256.to_string()
            )
        );
    }

    #[tokio::test]
    async fn foreign_database_without_metadata_is_rejected_without_changing_bytes() {
        let temporary = tempfile::tempdir().unwrap();
        let database = temporary.path().join("foreign.sqlite3");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE unrelated_records(id TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO unrelated_records VALUES('record', 'foreign');",
            )
            .unwrap();
        drop(connection);

        assert_rejected_without_byte_changes(&database).await;
    }

    #[tokio::test]
    async fn noncurrent_wal_generation_is_rejected_without_changing_bytes() {
        let temporary = tempfile::tempdir().unwrap();
        let database = temporary.path().join("noncurrent-wal.sqlite3");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA wal_autocheckpoint=0;
                 CREATE TABLE users(id TEXT PRIMARY KEY, username TEXT NOT NULL);
                 INSERT INTO users VALUES('unknown-user', 'unknown-admin');",
            )
            .unwrap();
        assert!(sqlite_sidecar(&database, "-wal").exists());

        // Keep the writer open so the committed schema exists only in the WAL
        // while the product performs its read-only current-schema rejection.
        assert_rejected_without_byte_changes(&database).await;
        drop(connection);
    }

    #[tokio::test]
    async fn corrupt_global_lease_wal_generations_are_read_only_restart_rejections() {
        let cases = [
            ("missing-row", "DELETE FROM media_reconciler_leases;"),
            (
                "extra-row",
                "INSERT INTO media_reconciler_leases (singleton, updated_at)
                 VALUES (2, '1970-01-01T00:00:00+00:00');",
            ),
            (
                "owner-without-expiry",
                "UPDATE media_reconciler_leases
                 SET lease_owner = '00000000-0000-4000-8000-000000000001';",
            ),
            (
                "noncanonical-owner",
                "UPDATE media_reconciler_leases
                 SET lease_owner = '00000000-0000-1000-8000-000000000001',
                     lease_expires_at = '2030-01-01T00:01:00+00:00',
                     updated_at = '2030-01-01T00:00:00+00:00';",
            ),
            (
                "invalid-time-relation",
                "UPDATE media_reconciler_leases
                 SET lease_owner = '00000000-0000-4000-8000-000000000001',
                     lease_expires_at = '2030-01-01T00:00:00+00:00',
                     updated_at = '2030-01-01T00:00:00+00:00';",
            ),
            (
                "unknown-shape",
                "ALTER TABLE media_reconciler_leases ADD COLUMN unexpected TEXT;",
            ),
        ];

        for (name, mutation) in cases {
            let temporary = tempfile::tempdir().unwrap();
            let database = temporary.path().join(format!("{name}.sqlite3"));
            initialize_current_database(&database).unwrap();
            let connection = Connection::open(&database).unwrap();
            connection
                .execute_batch(
                    "PRAGMA journal_mode=WAL;
                     PRAGMA wal_autocheckpoint=0;
                     PRAGMA ignore_check_constraints=ON;",
                )
                .unwrap();
            connection.execute_batch(mutation).unwrap();
            assert!(sqlite_sidecar(&database, "-wal").exists());

            // The second row is possible only because this fixture deliberately
            // models an out-of-protocol writer bypassing SQLite CHECK constraints.
            if name == "extra-row" {
                let count: i64 = connection
                    .query_row("SELECT COUNT(*) FROM media_reconciler_leases", [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                assert_eq!(count, 2);
            }
            assert_rejected_without_byte_changes(&database).await;
            drop(connection);
            assert_rejected_without_byte_changes(&database).await;
        }
    }

    #[test]
    fn current_schema_committed_only_in_wal_is_validated_without_changes() {
        let temporary = tempfile::tempdir().unwrap();
        let database = temporary.path().join("current-wal.sqlite3");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
            .unwrap();
        connection.execute_batch(CURRENT_SCHEMA).unwrap();
        let fingerprint = schema_fingerprint(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO product_metadata (
                     singleton, application, application_version, schema_revision, schema_sha256
                 ) VALUES (1, ?, ?, ?, ?)",
                params![
                    APPLICATION,
                    env!("CARGO_PKG_VERSION"),
                    CURRENT_SCHEMA_REVISION,
                    fingerprint
                ],
            )
            .unwrap();
        assert!(sqlite_sidecar(&database, "-wal").exists());

        let before = generation_bytes(&database);
        validate_current_database(&database).unwrap();
        assert!(
            generation_bytes(&database) == before,
            "current-schema validation changed SQLite generation bytes"
        );
        drop(connection);
    }

    #[tokio::test]
    async fn nonexact_metadata_and_actual_schema_are_read_only_rejections() {
        for (name, statement) in [
            (
                "wrong-application",
                "UPDATE product_metadata SET application = 'another-product'",
            ),
            (
                "noncurrent-version",
                "UPDATE product_metadata SET application_version = 'noncurrent-version'",
            ),
            (
                "wrong-revision",
                "UPDATE product_metadata SET schema_revision = 2",
            ),
            (
                "wrong-fingerprint",
                "UPDATE product_metadata SET schema_sha256 = 'ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff'",
            ),
            ("schema-tamper", "CREATE TABLE unexpected_product_table(id INTEGER)"),
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let database = temporary.path().join(format!("{name}.sqlite3"));
            initialize_current_database(&database).unwrap();
            let connection = Connection::open(&database).unwrap();
            connection.execute_batch("PRAGMA journal_mode=DELETE;").unwrap();
            connection.execute_batch(statement).unwrap();
            drop(connection);

            assert_rejected_without_byte_changes(&database).await;
        }
    }

    async fn assert_rejected_without_byte_changes(path: &Path) {
        let before = generation_bytes(path);
        let url = format!("sqlite://{}", path.display());
        let error = open_pool(&url).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("database")
                || format!("{error:#}").contains("product_metadata")
                || format!("{error:#}").contains("schema")
                || format!("{error:#}").contains("lease"),
            "rejection must identify the current-state boundary: {error:#}"
        );
        assert!(
            generation_bytes(path) == before,
            "current-schema rejection changed SQLite generation bytes"
        );
    }

    fn generation_bytes(path: &Path) -> BTreeMap<String, Vec<u8>> {
        sqlite_generation_paths(path)
            .into_iter()
            .filter(|candidate| candidate.exists())
            .map(|candidate| {
                (
                    candidate
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    fs::read(candidate).unwrap(),
                )
            })
            .collect()
    }
}
