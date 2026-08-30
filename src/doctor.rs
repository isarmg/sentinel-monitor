use crate::{
    crypto::{CredentialField, SecretBox},
    protocol::CONTRACT,
    runtime_lock::DatabaseMaintenanceLock,
    sqlite,
};
use anyhow::{ensure, Context};
use chrono::Utc;
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::Command,
    time::Duration,
};
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

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

struct ParsedContract {
    version: String,
    platform: String,
    sha256: String,
}

pub async fn run(options: &DoctorOptions) -> anyhow::Result<DoctorReport> {
    let _maintenance = DatabaseMaintenanceLock::shared(&options.database_url)?;
    let database = sqlite::database_path(&options.database_url)?;
    sqlite::validate_current_database(&database)?;
    sqlite::integrity_and_foreign_key_check(&database)?;
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
        let application = live_probe(&options.app_ready_url).await?;
        let mediamtx = live_probe(&options.mediamtx_ready_url).await?;
        ensure!(application, "application readiness endpoint is unavailable");
        ensure!(mediamtx, "MediaMTX readiness endpoint is unavailable");
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

fn verify_credentials(path: &Path, key: &[u8; 32]) -> anyhow::Result<()> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.busy_timeout(Duration::from_secs(5))?;
    let secret_box = SecretBox::new(key);
    let mut statement = connection.prepare(
        "SELECT id, main_stream_url_enc, sub_stream_url_enc, username_enc, password_enc \
         FROM cameras",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let camera_id = Uuid::parse_str(&row.get::<_, String>(0)?)
            .map_err(|_| anyhow::anyhow!("camera credential identity is invalid"))?;
        let main: Vec<u8> = row.get(1)?;
        let sub: Option<Vec<u8>> = row.get(2)?;
        let username: Option<Vec<u8>> = row.get(3)?;
        let password: Option<Vec<u8>> = row.get(4)?;
        for (field, value) in [
            (CredentialField::MainStreamUrl, Some(main)),
            (CredentialField::SubStreamUrl, sub),
            (CredentialField::Username, username),
            (CredentialField::Password, password),
        ] {
            if let Some(value) = value {
                secret_box.decrypt(camera_id, field, &value).map_err(|_| {
                    anyhow::anyhow!("CREDENTIALS_KEY cannot authenticate current camera envelopes")
                })?;
            }
        }
    }
    Ok(())
}

fn database_write_probe(path: &Path) -> anyhow::Result<()> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.execute_batch("PRAGMA foreign_keys=ON; BEGIN IMMEDIATE;")?;
    let probe = connection.execute(
        "INSERT INTO audit_logs (id, action, entity_type, details, created_at)
         VALUES (?, 'doctor_probe', 'system', '{}', ?)",
        (Uuid::new_v4().to_string(), Utc::now().to_rfc3339()),
    );
    let rollback = connection.execute_batch("ROLLBACK");
    probe.context("database write probe failed")?;
    rollback.context("roll back database write probe")?;
    Ok(())
}

fn recording_write_probe(root: &Path) -> anyhow::Result<()> {
    require_secure_directory(root, "recordings directory")?;
    let path = root.join(format!(".sentinel-doctor-{}", Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let mut file = options.open(&path)?;
    let result = (|| {
        file.write_all(b"sentinel-storage-probe")?;
        file.sync_all()?;
        let content = read_limited(&path, 64)?;
        ensure!(
            content == b"sentinel-storage-probe",
            "recording storage read/write probe failed"
        );
        Ok::<_, anyhow::Error>(())
    })();
    drop(file);
    let cleanup = fs::remove_file(&path).context("remove recording storage probe");
    sync_parent(&path)?;
    result?;
    cleanup
}

fn verify_companion(
    contract_path: &Path,
    binary_path: &Path,
    config_path: &Path,
    recordings_directory: &Path,
) -> anyhow::Result<()> {
    require_secure_file(contract_path, "MediaMTX contract")?;
    require_secure_file(binary_path, "MediaMTX binary")?;
    require_secure_file(config_path, "MediaMTX config")?;
    require_secure_directory(recordings_directory, "recordings directory")?;

    let contract = parse_contract(contract_path)?;
    ensure!(
        contract.platform == "linux_amd64",
        "MediaMTX companion platform is unsupported"
    );
    ensure!(
        sha256_file(binary_path)? == contract.sha256,
        "MediaMTX binary hash does not match its contract"
    );
    let output = Command::new(binary_path)
        .arg("--version")
        .output()
        .context("execute MediaMTX version check")?;
    ensure!(output.status.success(), "MediaMTX version check failed");
    ensure!(
        output.stdout.len() <= 256 && output.stderr.len() <= 256,
        "MediaMTX version output is unexpectedly large"
    );
    let version = String::from_utf8(output.stdout).context("MediaMTX version is not UTF-8")?;
    ensure!(
        version.trim() == contract.version,
        "MediaMTX binary version does not match its contract"
    );
    verify_media_config(config_path, recordings_directory)
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
        sha256.len() == 64
            && sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "MediaMTX SHA-256 must be lowercase hexadecimal"
    );
    Ok(ParsedContract {
        version,
        platform,
        sha256,
    })
}

fn verify_media_config(path: &Path, recordings_directory: &Path) -> anyhow::Result<()> {
    let content = String::from_utf8(read_limited(path, 1024 * 1024)?)
        .context("MediaMTX config is not UTF-8")?;
    let auth_address = format!("http://127.0.0.1:8080{}", CONTRACT.media_auth_path);
    for (key, expected) in [
        ("authMethod", "http"),
        ("authHTTPAddress", auth_address.as_str()),
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
    let paths = config_values(&content, "recordPath");
    ensure!(
        paths.len() == 1,
        "MediaMTX config must declare exactly one recordPath"
    );
    let configured = paths[0].trim_matches(['\'', '"']);
    let root = configured
        .split_once("%path")
        .map(|(prefix, _)| prefix.trim_end_matches('/'))
        .context("MediaMTX recordPath must contain a %path component")?;
    ensure!(!root.is_empty(), "MediaMTX recordPath has an empty root");
    let root = PathBuf::from(root);
    ensure!(root.is_absolute(), "MediaMTX recordPath must be absolute");
    ensure!(
        fs::canonicalize(&root)? == fs::canonicalize(recordings_directory)?,
        "MediaMTX config points at a different recordings directory"
    );
    Ok(())
}

fn config_values<'a>(content: &'a str, key: &str) -> Vec<&'a str> {
    let prefix = format!("{key}:");
    content
        .lines()
        .filter_map(|line| line.trim().strip_prefix(&prefix).map(str::trim))
        .collect()
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut file = open_read_no_follow(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(lower_hex(hasher.finalize()))
}

fn lower_hex(bytes: impl AsRef<[u8]>) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(bytes.as_ref().len() * 2);
    for byte in bytes.as_ref() {
        write!(&mut output, "{byte:02x}").expect("write to String");
    }
    output
}

fn read_limited(path: &Path, limit: u64) -> anyhow::Result<Vec<u8>> {
    require_secure_file(path, "input file")?;
    let before = fs::symlink_metadata(path)?;
    ensure!(before.len() <= limit, "input file exceeds its size limit");
    let mut output = Vec::with_capacity(before.len() as usize);
    open_read_no_follow(path)?.read_to_end(&mut output)?;
    let after = fs::symlink_metadata(path)?;
    ensure!(
        stable_metadata(&before, &after) && output.len() as u64 == after.len(),
        "input file changed while it was read"
    );
    Ok(output)
}

fn require_secure_file(path: &Path, description: &str) -> anyhow::Result<()> {
    reject_symlink_components(path)?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("{description} does not exist: {}", path.display()))?;
    ensure!(metadata.is_file(), "{description} must be a regular file");
    #[cfg(unix)]
    ensure!(
        metadata.nlink() == 1,
        "{description} must not have hard-link aliases"
    );
    Ok(())
}

fn require_secure_directory(path: &Path, description: &str) -> anyhow::Result<()> {
    reject_symlink_components(path)?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("{description} does not exist: {}", path.display()))?;
    ensure!(metadata.is_dir(), "{description} must be a directory");
    Ok(())
}

fn reject_symlink_components(path: &Path) -> anyhow::Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => current.push("."),
            Component::Normal(value) => current.push(value),
            Component::ParentDir => anyhow::bail!("operational path must not contain traversal"),
        }
        let metadata = fs::symlink_metadata(&current)
            .with_context(|| format!("operational path does not exist: {}", current.display()))?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "symbolic links are not accepted in operational paths"
        );
    }
    Ok(())
}

fn open_read_no_follow(path: &Path) -> anyhow::Result<File> {
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
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.mtime_nsec() == after.mtime_nsec()
    }
    #[cfg(not(unix))]
    true
}

fn sync_parent(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    File::open(path.parent().unwrap_or_else(|| Path::new(".")))?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn offline_doctor_checks_current_database_storage_companion_and_credentials() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let database = temporary.path().join("sentinel.sqlite3");
        let database_url = format!("sqlite://{}", database.display());
        let pool = sqlite::open_pool(&database_url).await.unwrap();
        let key = [0x42; 32];
        let now = Utc::now().to_rfc3339();
        let user = Uuid::new_v4().to_string();
        let camera_id = Uuid::new_v4();
        let secret_box = SecretBox::new(&key);
        let encrypted = secret_box
            .encrypt(
                camera_id,
                CredentialField::MainStreamUrl,
                "rtsp://camera.invalid/main",
            )
            .unwrap();
        let username = secret_box
            .encrypt(camera_id, CredentialField::Username, "doctor-camera-user")
            .unwrap();
        let password = secret_box
            .encrypt(
                camera_id,
                CredentialField::Password,
                "doctor-camera-password",
            )
            .unwrap();
        sqlx::query(
            "INSERT INTO users (id, email, password_hash, role, created_at, updated_at)
             VALUES (?, 'doctor@example.test', 'hash', 'admin', ?, ?)",
        )
        .bind(&user)
        .bind(&now)
        .bind(&now)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO cameras (id, name, main_stream_url_enc, username_enc, password_enc, \
             created_by, created_at, updated_at) VALUES (?, 'Doctor', ?, ?, ?, ?, ?, ?)",
        )
        .bind(camera_id.to_string())
        .bind(encrypted)
        .bind(username)
        .bind(password)
        .bind(&user)
        .bind(&now)
        .bind(&now)
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let recordings = temporary.path().join("recordings");
        fs::create_dir(&recordings).unwrap();
        let binary = temporary.path().join("mediamtx");
        let companion_marker = temporary.path().join("companion-executed");
        fs::write(
            &binary,
            format!(
                "#!/bin/sh\nprintf touched > '{}'\nprintf 'v1.20.0\\n'\n",
                companion_marker.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let contract = temporary.path().join("mediamtx.lock");
        fs::write(
            &contract,
            format!(
                "version=v1.20.0\nplatform=linux_amd64\nsha256={}\n",
                sha256_file(&binary).unwrap()
            ),
        )
        .unwrap();
        let config = temporary.path().join("mediamtx.yml");
        fs::write(
            &config,
            format!(
                "authMethod: http\n\
                 authHTTPAddress: http://127.0.0.1:8080/internal/v2/media/auth\n\
                 apiAddress: 127.0.0.1:9997\n\
                 playbackAddress: 127.0.0.1:9996\n\
                 recordFormat: fmp4\n\
                 recordPath: {}/%path/%Y-%m-%d.mp4\n",
                recordings.display()
            ),
        )
        .unwrap();

        let options = DoctorOptions {
            database_url: database_url.clone(),
            mediamtx_config: config.clone(),
            mediamtx_contract: contract.clone(),
            mediamtx_binary: binary.clone(),
            recordings_directory: recordings.clone(),
            credentials_key: key,
            app_ready_url: "http://127.0.0.1:1/health/ready".to_string(),
            mediamtx_ready_url: "http://127.0.0.1:1/v3/info".to_string(),
            offline: true,
        };
        let report = run(&options).await.unwrap();
        assert_eq!(report.status, "ok");
        assert!(companion_marker.exists());
        fs::remove_file(&companion_marker).unwrap();
        assert!(!fs::read_dir(&recordings).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".sentinel-doctor-")));

        let wrong_key = DoctorOptions {
            credentials_key: [0x99; 32],
            ..options.clone()
        };
        let wrong_key_error = run(&wrong_key).await.unwrap_err().to_string();
        assert!(!wrong_key_error.contains("doctor-camera-user"));
        assert!(!wrong_key_error.contains("doctor-camera-password"));
        assert!(!wrong_key_error.contains("rtsp://"));
        assert!(!companion_marker.exists());

        let connection = Connection::open(&database).unwrap();
        connection.busy_timeout(Duration::from_secs(5)).unwrap();
        connection
            .execute(
                "UPDATE cameras SET username_enc = password_enc, password_enc = username_enc \
                 WHERE id = ?",
                [camera_id.to_string()],
            )
            .unwrap();
        connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        drop(connection);
        let before = sha256_file(&database).unwrap();
        let tampered_error = run(&options).await.unwrap_err().to_string();
        assert_eq!(sha256_file(&database).unwrap(), before);
        assert!(!tampered_error.contains("doctor-camera-user"));
        assert!(!tampered_error.contains("doctor-camera-password"));
        assert!(!tampered_error.contains("rtsp://"));
        assert!(!companion_marker.exists());
        assert!(live_probe("http://example.com/health/ready").await.is_err());
    }
}
