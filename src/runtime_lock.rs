use anyhow::{ensure, Context};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};

pub struct ApplicationLock {
    _lock: File,
    pid_path: PathBuf,
    pid: u32,
}

impl ApplicationLock {
    pub fn acquire(runtime_directory: &Path) -> anyhow::Result<Self> {
        require_real_directory(runtime_directory)?;
        let lock_path = runtime_directory.join("app.lock");
        reject_symlink(&lock_path)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let lock = options
            .open(&lock_path)
            .with_context(|| format!("open application lock {}", lock_path.display()))?;
        acquire_exclusive(&lock).context(
            "another Sentinel instance already owns this runtime directory; refusing to start",
        )?;

        let pid = std::process::id();
        let pid_path = runtime_directory.join("app.pid");
        write_pid(&pid_path, pid)?;
        Ok(Self {
            _lock: lock,
            pid_path,
            pid,
        })
    }
}

impl Drop for ApplicationLock {
    fn drop(&mut self) {
        let owned = fs::read_to_string(&self.pid_path)
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            == Some(self.pid);
        if owned {
            let _ = fs::remove_file(&self.pid_path);
            let _ = sync_parent(&self.pid_path);
        }
    }
}

fn write_pid(path: &Path, pid: u32) -> anyhow::Result<()> {
    reject_symlink(path)?;
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = options
        .open(path)
        .with_context(|| format!("write application PID file {}", path.display()))?;
    writeln!(file, "{pid}")?;
    file.sync_all()?;
    sync_parent(path)?;
    Ok(())
}

fn require_real_directory(path: &Path) -> anyhow::Result<()> {
    reject_symlink_components(path)?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("runtime directory does not exist: {}", path.display()))?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "runtime directory must be a real directory"
    );
    Ok(())
}

fn reject_symlink_components(path: &Path) -> anyhow::Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) => ensure!(
                !metadata.file_type().is_symlink(),
                "runtime directory path must not traverse symbolic links"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn reject_symlink(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            !metadata.file_type().is_symlink(),
            "runtime lock paths must not be symbolic links"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn acquire_exclusive(file: &File) -> anyhow::Result<()> {
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
        anyhow::bail!("Sentinel runtime locking is supported only on Unix")
    }
}

fn sync_parent(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    File::open(path.parent().unwrap_or(Path::new(".")))?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_instance_is_refused_for_the_same_runtime() {
        let runtime = tempfile::tempdir().unwrap();
        let first = ApplicationLock::acquire(runtime.path()).unwrap();
        assert_eq!(
            fs::read_to_string(runtime.path().join("app.pid"))
                .unwrap()
                .trim(),
            std::process::id().to_string()
        );
        assert!(ApplicationLock::acquire(runtime.path()).is_err());
        drop(first);
        assert!(!runtime.path().join("app.pid").exists());
        ApplicationLock::acquire(runtime.path()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn runtime_and_lock_symlinks_are_rejected() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let real = temporary.path().join("real");
        fs::create_dir(&real).unwrap();
        let linked = temporary.path().join("linked");
        symlink(&real, &linked).unwrap();
        assert!(ApplicationLock::acquire(&linked).is_err());

        let parent = temporary.path().join("parent");
        let nested_runtime = parent.join("runtime");
        fs::create_dir_all(&nested_runtime).unwrap();
        let linked_parent = temporary.path().join("linked-parent");
        symlink(&parent, &linked_parent).unwrap();
        assert!(ApplicationLock::acquire(&linked_parent.join("runtime")).is_err());

        symlink(temporary.path().join("outside"), real.join("app.lock")).unwrap();
        assert!(ApplicationLock::acquire(&real).is_err());
    }
}
