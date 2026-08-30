use anyhow::{ensure, Context};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
};

#[cfg(unix)]
use std::os::{
    fd::AsRawFd,
    unix::fs::{MetadataExt, OpenOptionsExt},
};

/// Locks held for the complete Sentinel process lifetime.
///
/// The database-adjacent instance lock fences the database identity even when
/// two processes were configured with different runtime directories. The
/// shared maintenance lock permits read-only/online maintenance while excluding
/// replacement of the live database inode. The runtime lock remains necessary
/// for the PID and native launcher contract.
pub struct ApplicationLock {
    _database_instance: File,
    _database_maintenance: File,
    _runtime: File,
    database: DatabaseTarget,
    pid_path: PathBuf,
    pid_identity: FileIdentity,
    pid: u32,
}

pub struct DatabaseMaintenanceLock {
    _lock: File,
}

impl ApplicationLock {
    pub fn acquire(database_url: &str, runtime_directory: &Path) -> anyhow::Result<Self> {
        let database = DatabaseTarget::from_url(database_url)?;
        let paths = DatabaseLockPaths::for_target(&database)?;
        let database_instance = acquire_lock(&paths.instance, LockKind::Exclusive).context(
            "another Sentinel instance already owns this SQLite database; refusing to start",
        )?;
        let database_maintenance = acquire_lock(&paths.maintenance, LockKind::Shared)
            .context("exclusive Sentinel database maintenance is active; refusing to start")?;
        database.ensure_unchanged()?;

        require_real_directory(runtime_directory, "runtime directory")?;
        let runtime_path = runtime_directory.join("app.lock");
        let runtime = acquire_lock(&runtime_path, LockKind::Exclusive).context(
            "another Sentinel instance already owns this runtime directory; refusing to start",
        )?;

        let pid = std::process::id();
        let pid_path = runtime_directory.join("app.pid");
        let pid_identity = write_pid(&pid_path, pid)?;
        Ok(Self {
            _database_instance: database_instance,
            _database_maintenance: database_maintenance,
            _runtime: runtime,
            database,
            pid_path,
            pid_identity,
            pid,
        })
    }

    /// Recheck after SQLite opens the file. This closes the absent-file and
    /// path-replacement window between acquiring the locks and creating a new
    /// database on first start.
    pub fn validate_open_database(&self) -> anyhow::Result<()> {
        self.database.validate_open_database()
    }
}

impl DatabaseMaintenanceLock {
    pub fn shared(database_url: &str) -> anyhow::Result<Self> {
        Self::acquire(database_url, LockKind::Shared)
            .context("exclusive Sentinel database maintenance is active")
    }

    pub fn exclusive(database_url: &str) -> anyhow::Result<Self> {
        Self::acquire(database_url, LockKind::Exclusive)
            .context("Sentinel is running or another database maintenance command is active")
    }

    fn acquire(database_url: &str, kind: LockKind) -> anyhow::Result<Self> {
        let database = DatabaseTarget::from_url(database_url)?;
        let path = DatabaseLockPaths::for_target(&database)?.maintenance;
        let lock = acquire_lock(&path, kind)?;
        database.ensure_unchanged()?;
        Ok(Self { _lock: lock })
    }
}

impl Drop for ApplicationLock {
    fn drop(&mut self) {
        let identity_is_ours = secure_regular_identity(&self.pid_path, "application PID file")
            .ok()
            .flatten()
            == Some(self.pid_identity);
        let contents_are_ours = fs::read_to_string(&self.pid_path)
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            == Some(self.pid);
        if identity_is_ours && contents_are_ours {
            let _ = fs::remove_file(&self.pid_path);
            let _ = sync_parent(&self.pid_path);
        }
    }
}

#[derive(Clone, Copy)]
enum LockKind {
    Shared,
    Exclusive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        }
    }
}

struct DatabaseTarget {
    path: PathBuf,
    initial_identity: Option<FileIdentity>,
}

impl DatabaseTarget {
    fn from_url(database_url: &str) -> anyhow::Result<Self> {
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
        ensure!(
            path.is_absolute(),
            "SQLite database path must be absolute for identity locking"
        );
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
        require_real_directory(real_parent(&path), "SQLite database directory")?;
        let initial_identity = secure_regular_identity(&path, "SQLite database")?;
        Ok(Self {
            path,
            initial_identity,
        })
    }

    fn ensure_unchanged(&self) -> anyhow::Result<()> {
        ensure!(
            secure_regular_identity(&self.path, "SQLite database")? == self.initial_identity,
            "SQLite database path changed while acquiring its runtime lock"
        );
        Ok(())
    }

    fn validate_open_database(&self) -> anyhow::Result<()> {
        let current = secure_regular_identity(&self.path, "SQLite database")?
            .context("SQLite did not open a regular single-link database file")?;
        if let Some(initial) = self.initial_identity {
            ensure!(
                current == initial,
                "SQLite database inode changed after the runtime lock was acquired"
            );
        }
        Ok(())
    }
}

struct DatabaseLockPaths {
    instance: PathBuf,
    maintenance: PathBuf,
}

impl DatabaseLockPaths {
    fn for_target(database: &DatabaseTarget) -> anyhow::Result<Self> {
        let name = database
            .path
            .file_name()
            .context("SQLite database path must name a file")?;
        let parent = real_parent(&database.path);
        Ok(Self {
            instance: parent.join(lock_name(name, ".sentinel-monitor.instance.lock")),
            maintenance: parent.join(lock_name(name, ".sentinel-monitor.maintenance.lock")),
        })
    }
}

fn lock_name(database_name: &std::ffi::OsStr, suffix: &str) -> OsString {
    let mut name = OsString::from(".");
    name.push(database_name);
    name.push(suffix);
    name
}

fn acquire_lock(path: &Path, kind: LockKind) -> anyhow::Result<File> {
    require_real_directory(real_parent(path), "lock directory")?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options
        .open(path)
        .with_context(|| format!("open runtime lock {}", path.display()))?;
    validate_open_lock(path, &file)?;
    acquire_flock(&file, kind)?;
    Ok(file)
}

fn validate_open_lock(path: &Path, file: &File) -> anyhow::Result<()> {
    let opened = file.metadata()?;
    let named = fs::symlink_metadata(path)?;
    ensure!(
        opened.is_file() && named.is_file() && !named.file_type().is_symlink(),
        "runtime lock must be a regular file without symbolic links"
    );
    #[cfg(unix)]
    {
        ensure!(
            opened.nlink() == 1 && named.nlink() == 1,
            "runtime lock must not have hard-link aliases"
        );
        ensure!(
            FileIdentity::from_metadata(&opened) == FileIdentity::from_metadata(&named),
            "runtime lock path changed while it was opened"
        );
        ensure!(
            opened.mode() & 0o077 == 0,
            "runtime lock permissions must not grant group or other access"
        );
    }
    Ok(())
}

fn acquire_flock(file: &File, kind: LockKind) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let operation = match kind {
            LockKind::Shared => libc::LOCK_SH | libc::LOCK_NB,
            LockKind::Exclusive => libc::LOCK_EX | libc::LOCK_NB,
        };
        let result = unsafe { libc::flock(file.as_raw_fd(), operation) };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (file, kind);
        anyhow::bail!("Sentinel runtime locking is supported only on Unix")
    }
}

fn write_pid(path: &Path, pid: u32) -> anyhow::Result<FileIdentity> {
    require_real_directory(real_parent(path), "runtime directory")?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = options
        .open(path)
        .with_context(|| format!("write application PID file {}", path.display()))?;
    validate_open_lock(path, &file).context("validate application PID file")?;
    file.set_len(0)?;
    writeln!(file, "{pid}")?;
    file.sync_all()?;
    sync_parent(path)?;
    Ok(FileIdentity::from_metadata(&file.metadata()?))
}

fn secure_regular_identity(path: &Path, description: &str) -> anyhow::Result<Option<FileIdentity>> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "{description} must be a regular file without symbolic links"
            );
            #[cfg(unix)]
            ensure!(
                metadata.nlink() == 1,
                "{description} must not have hard-link aliases"
            );
            Ok(Some(FileIdentity::from_metadata(&metadata)))
        }
    }
}

fn require_real_directory(path: &Path, description: &str) -> anyhow::Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => current.push("."),
            Component::Normal(value) => current.push(value),
            Component::ParentDir => {
                anyhow::bail!("{description} path must not contain parent traversal")
            }
        }
        let metadata = fs::symlink_metadata(&current)
            .with_context(|| format!("{description} does not exist: {}", current.display()))?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "{description} path must not traverse symbolic links or special files"
        );
    }
    Ok(())
}

fn real_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn sync_parent(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    File::open(path.parent().unwrap_or(Path::new(".")))?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::{process::Command, thread, time::Duration};

    fn database_url(path: &Path) -> String {
        format!("sqlite://{}", path.display())
    }

    fn create_wal_database(path: &Path) -> Connection {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;\
                 PRAGMA wal_autocheckpoint=0;\
                 CREATE TABLE runtime_lock_probe(id INTEGER PRIMARY KEY);\
                 INSERT INTO runtime_lock_probe(id) VALUES(1);",
            )
            .unwrap();
        connection
    }

    #[test]
    fn database_identity_fences_different_runtimes_and_survives_wal_restart() {
        let temporary = tempfile::tempdir().unwrap();
        let first_runtime = temporary.path().join("runtime-one");
        let second_runtime = temporary.path().join("runtime-two");
        fs::create_dir(&first_runtime).unwrap();
        fs::create_dir(&second_runtime).unwrap();
        let database = temporary.path().join("app.sqlite3");
        let writer = create_wal_database(&database);
        let url = database_url(&database);
        let ready = temporary.path().join("child-ready");
        let release = temporary.path().join("child-release");

        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "runtime_lock::tests::application_lock_child",
                "--nocapture",
            ])
            .env("SENTINEL_LOCK_TEST_DATABASE_URL", &url)
            .env("SENTINEL_LOCK_TEST_RUNTIME", &first_runtime)
            .env("SENTINEL_LOCK_TEST_READY", &ready)
            .env("SENTINEL_LOCK_TEST_RELEASE", &release)
            .spawn()
            .unwrap();
        wait_for_file(&ready);

        assert!(sqlite_sidecar(&database, "-wal").is_file());
        assert!(ApplicationLock::acquire(&url, &second_runtime).is_err());
        assert!(DatabaseMaintenanceLock::exclusive(&url).is_err());
        DatabaseMaintenanceLock::shared(&url).unwrap();

        drop(writer);
        fs::write(&release, b"release").unwrap();
        assert!(child.wait().unwrap().success());
        let maintenance = DatabaseMaintenanceLock::exclusive(&url).unwrap();
        assert!(ApplicationLock::acquire(&url, &second_runtime).is_err());
        drop(maintenance);

        let restarted = ApplicationLock::acquire(&url, &second_runtime).unwrap();
        restarted.validate_open_database().unwrap();
        let reopened = Connection::open(&database).unwrap();
        let count: i64 = reopened
            .query_row("SELECT COUNT(*) FROM runtime_lock_probe", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    #[ignore = "subprocess helper for the cross-process database lock test"]
    fn application_lock_child() {
        let Some(database_url) = std::env::var_os("SENTINEL_LOCK_TEST_DATABASE_URL") else {
            return;
        };
        let runtime = PathBuf::from(std::env::var_os("SENTINEL_LOCK_TEST_RUNTIME").unwrap());
        let ready = PathBuf::from(std::env::var_os("SENTINEL_LOCK_TEST_READY").unwrap());
        let release = PathBuf::from(std::env::var_os("SENTINEL_LOCK_TEST_RELEASE").unwrap());
        let database_url = database_url.into_string().unwrap();
        let lock = ApplicationLock::acquire(&database_url, &runtime).unwrap();
        lock.validate_open_database().unwrap();
        fs::write(&ready, b"ready").unwrap();
        for _ in 0..1_000 {
            if release.exists() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("parent did not release the lock helper");
    }

    fn wait_for_file(path: &Path) {
        for _ in 0..1_000 {
            if path.exists() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("subprocess did not acquire the database lock");
    }

    #[test]
    fn second_instance_is_refused_for_the_same_runtime() {
        let temporary = tempfile::tempdir().unwrap();
        let runtime = temporary.path().join("runtime");
        fs::create_dir(&runtime).unwrap();
        let database = temporary.path().join("app.sqlite3");
        let connection = create_wal_database(&database);
        drop(connection);
        let url = database_url(&database);

        let first = ApplicationLock::acquire(&url, &runtime).unwrap();
        assert_eq!(
            fs::read_to_string(runtime.join("app.pid")).unwrap().trim(),
            std::process::id().to_string()
        );
        assert!(ApplicationLock::acquire(&url, &runtime).is_err());
        drop(first);
        assert!(!runtime.join("app.pid").exists());
        ApplicationLock::acquire(&url, &runtime).unwrap();
    }

    fn sqlite_sidecar(path: &Path, suffix: &str) -> PathBuf {
        let mut value = path.as_os_str().to_os_string();
        value.push(suffix);
        PathBuf::from(value)
    }

    #[cfg(unix)]
    #[test]
    fn database_and_lock_aliases_fail_closed() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let runtime = temporary.path().join("runtime");
        fs::create_dir(&runtime).unwrap();
        let database = temporary.path().join("app.sqlite3");
        drop(create_wal_database(&database));

        let database_symlink = temporary.path().join("database-symlink.sqlite3");
        symlink(&database, &database_symlink).unwrap();
        assert!(ApplicationLock::acquire(&database_url(&database_symlink), &runtime).is_err());

        let database_hardlink = temporary.path().join("database-hardlink.sqlite3");
        fs::hard_link(&database, &database_hardlink).unwrap();
        assert!(ApplicationLock::acquire(&database_url(&database), &runtime).is_err());
        assert!(ApplicationLock::acquire(&database_url(&database_hardlink), &runtime).is_err());
        fs::remove_file(&database_hardlink).unwrap();

        let url = database_url(&database);
        let first = ApplicationLock::acquire(&url, &runtime).unwrap();
        drop(first);
        let lock_path = DatabaseLockPaths::for_target(&DatabaseTarget::from_url(&url).unwrap())
            .unwrap()
            .instance;
        let lock_alias = temporary.path().join("instance-lock-alias");
        fs::hard_link(&lock_path, &lock_alias).unwrap();
        assert!(ApplicationLock::acquire(&url, &runtime).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn runtime_database_parents_and_lock_symlinks_are_rejected() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let real_runtime = temporary.path().join("real-runtime");
        fs::create_dir(&real_runtime).unwrap();
        let linked_runtime = temporary.path().join("linked-runtime");
        symlink(&real_runtime, &linked_runtime).unwrap();
        let database = temporary.path().join("app.sqlite3");
        drop(create_wal_database(&database));
        let url = database_url(&database);
        assert!(ApplicationLock::acquire(&url, &linked_runtime).is_err());

        let real_database_directory = temporary.path().join("real-data");
        fs::create_dir(&real_database_directory).unwrap();
        let nested_database = real_database_directory.join("nested.sqlite3");
        drop(create_wal_database(&nested_database));
        let linked_database_directory = temporary.path().join("linked-data");
        symlink(&real_database_directory, &linked_database_directory).unwrap();
        assert!(ApplicationLock::acquire(
            &database_url(&linked_database_directory.join("nested.sqlite3")),
            &real_runtime,
        )
        .is_err());

        symlink(
            temporary.path().join("outside"),
            real_runtime.join("app.lock"),
        )
        .unwrap();
        assert!(ApplicationLock::acquire(&url, &real_runtime).is_err());
    }
}
