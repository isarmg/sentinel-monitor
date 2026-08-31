use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Component, Path, PathBuf},
};

use anyhow::{bail, ensure, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

use crate::crypto::{credential_contract_sha256, CREDENTIAL_ENVELOPE_REVISION};

const MANIFEST_FORMAT: &str = "sentinel-release-v2";
const PRODUCT: &str = "sentinel-monitor";
const VERSION: &str = "0.2.0";
const TARGET: &str = "x86_64-unknown-linux-gnu";
const SERVER_BINARY: &str = "bin/sentinel-monitor";
const MANIFEST_NAME: &str = "RELEASE-MANIFEST";
const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_RELEASE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 10_000;
pub(crate) const PRODUCTION_RELEASE_ROOT: &str = "/opt/isarmg/sentinel-monitor/releases/0.2.0";
const RELOCATABLE_RELEASE_SUFFIX: &str = "opt/isarmg/sentinel-monitor/releases/0.2.0";
// config/ 是仓库内受审配置的唯一位置；发布包仍按运行时契约写入 config/。
const MEDIAMTX_LOCK: &[u8] = include_bytes!("../config/mediamtx.lock");
const MEDIAMTX_CONFIG: &[u8] = include_bytes!("../config/mediamtx.yml");

const FIXED_DIRECTORIES: &[&str] = &["bin", "config", "native", "web", "web/assets"];
const FIXED_FILES: &[(&str, u32)] = &[
    ("bin/mediamtx", 0o555),
    (SERVER_BINARY, 0o555),
    ("config/mediamtx.lock", 0o444),
    ("config/mediamtx.yml", 0o444),
    ("native/bootstrap.sh", 0o555),
    ("native/common.sh", 0o555),
    ("native/start.sh", 0o555),
    ("native/status.sh", 0o555),
    ("native/stop.sh", 0o555),
    ("web/index.html", 0o444),
];

#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub(crate) struct ReleaseIdentity {
    manifest_format: &'static str,
    application: &'static str,
    application_version: &'static str,
    source_revision: &'static str,
    target: &'static str,
    wire_protocol: String,
    api_prefix: String,
    schema_revision: i64,
    schema_sha256: &'static str,
    credential_envelope_revision: u32,
    credential_contract_sha256: String,
    static_contract_sha256: String,
    mediamtx_version: String,
    mediamtx_platform: String,
    mediamtx_sha256: String,
    mediamtx_config_sha256: String,
    release_contract_sha256: String,
}

#[derive(Debug)]
struct MediaMtxIdentity {
    version: String,
    platform: String,
    sha256: String,
}

#[derive(Clone, Debug)]
enum ManifestEntry {
    Directory {
        mode: u32,
    },
    File {
        mode: u32,
        size: u64,
        sha256: String,
    },
}

#[derive(Debug)]
struct FileHash {
    size: u64,
    sha256: String,
}

#[derive(Default)]
struct Counters {
    entries: usize,
    bytes: u64,
}

pub(crate) fn identity() -> Result<ReleaseIdentity> {
    let media = mediamtx_identity()?;
    let credential_contract_sha256 = credential_contract_sha256();
    let static_contract_sha256 = crate::static_assets::embedded_manifest_sha256();
    let mediamtx_config_sha256 = sha256_hex(MEDIAMTX_CONFIG);
    let contract = format!(
        "manifest_format={MANIFEST_FORMAT}\napplication={PRODUCT}\napplication_version={VERSION}\ntarget={}\nwire_protocol={}\napi_prefix={}\nschema_revision={}\nschema_sha256={}\ncredential_envelope_revision={CREDENTIAL_ENVELOPE_REVISION}\ncredential_contract_sha256={credential_contract_sha256}\nstatic_contract_sha256={static_contract_sha256}\nmediamtx_version={}\nmediamtx_platform={}\nmediamtx_sha256={}\nmediamtx_config_sha256={mediamtx_config_sha256}\n",
        env!("SENTINEL_BUILD_TARGET"),
        crate::protocol::CONTRACT.wire_protocol,
        crate::protocol::CONTRACT.api_prefix,
        crate::sqlite::CURRENT_SCHEMA_REVISION,
        crate::sqlite::CURRENT_SCHEMA_SHA256,
        media.version,
        media.platform,
        media.sha256,
    );
    Ok(ReleaseIdentity {
        manifest_format: MANIFEST_FORMAT,
        application: PRODUCT,
        application_version: VERSION,
        source_revision: env!("SENTINEL_SOURCE_REVISION"),
        target: env!("SENTINEL_BUILD_TARGET"),
        wire_protocol: crate::protocol::CONTRACT.wire_protocol.clone(),
        api_prefix: crate::protocol::CONTRACT.api_prefix.clone(),
        schema_revision: crate::sqlite::CURRENT_SCHEMA_REVISION,
        schema_sha256: crate::sqlite::CURRENT_SCHEMA_SHA256,
        credential_envelope_revision: CREDENTIAL_ENVELOPE_REVISION,
        credential_contract_sha256,
        static_contract_sha256,
        mediamtx_version: media.version,
        mediamtx_platform: media.platform,
        mediamtx_sha256: media.sha256,
        mediamtx_config_sha256,
        release_contract_sha256: sha256_hex(contract.as_bytes()),
    })
}

pub(crate) fn identity_json() -> Result<String> {
    serde_json::to_string(&identity()?).context("serialize Sentinel release identity")
}

pub(crate) fn manifest_header() -> Result<String> {
    let identity = identity()?;
    Ok(format!(
        "format={}\napplication={}\napplication_version={}\nsource_revision={}\ntarget={}\nwire_protocol={}\napi_prefix={}\nschema_revision={}\nschema_sha256={}\ncredential_envelope_revision={}\ncredential_contract_sha256={}\nstatic_contract_sha256={}\nmediamtx_version={}\nmediamtx_platform={}\nmediamtx_sha256={}\nmediamtx_config_sha256={}\nrelease_contract_sha256={}\n",
        identity.manifest_format,
        identity.application,
        identity.application_version,
        identity.source_revision,
        identity.target,
        identity.wire_protocol,
        identity.api_prefix,
        identity.schema_revision,
        identity.schema_sha256,
        identity.credential_envelope_revision,
        identity.credential_contract_sha256,
        identity.static_contract_sha256,
        identity.mediamtx_version,
        identity.mediamtx_platform,
        identity.mediamtx_sha256,
        identity.mediamtx_config_sha256,
        identity.release_contract_sha256,
    ))
}

pub(crate) fn ensure_unbound_development_serve() -> Result<()> {
    ensure!(
        env!("SENTINEL_SOURCE_REVISION") == "unbound",
        "a source-bound Sentinel release cannot use ordinary serve; use serve-release RELEASE_ROOT"
    );
    Ok(())
}

pub(crate) fn verify_release(root: &Path) -> Result<ReleaseIdentity> {
    verify_release_with_options(root, true, true)
}

fn verify_release_with_options(
    root: &Path,
    require_current_executable: bool,
    require_bound_source: bool,
) -> Result<ReleaseIdentity> {
    let root = validate_release_root(root)?;
    let production = root == Path::new(PRODUCTION_RELEASE_ROOT);
    if production {
        for directory in [
            "/opt",
            "/opt/isarmg",
            "/opt/isarmg/sentinel-monitor",
            "/opt/isarmg/sentinel-monitor/releases",
        ] {
            require_directory(
                Path::new(directory),
                0o755,
                true,
                "production release parent",
            )?;
        }
    }
    require_directory(&root, 0o555, production, "release root")?;

    let identity = identity()?;
    if require_bound_source {
        ensure!(
            is_source_revision(identity.source_revision),
            "release binary has no exact source revision binding"
        );
    }
    ensure!(
        identity.target == TARGET,
        "unsupported release build target"
    );

    if require_current_executable {
        let executing = fs::canonicalize(
            std::env::current_exe().context("resolve executing Sentinel binary")?,
        )?;
        ensure!(
            executing == root.join(SERVER_BINARY),
            "release must be verified by the Sentinel binary physically contained by RELEASE_ROOT"
        );
    }

    let manifest_path = root.join(MANIFEST_NAME);
    let manifest_bytes = read_small_regular_file(
        &manifest_path,
        0o444,
        MAX_MANIFEST_BYTES,
        production,
        "release manifest",
    )?;
    let manifest =
        std::str::from_utf8(&manifest_bytes).context("release manifest must be canonical UTF-8")?;
    let expected_header = manifest_header()?;
    let records = manifest
        .strip_prefix(&expected_header)
        .context("release manifest identity differs from the executing binary")?;
    let expected = parse_entries(records)?;
    validate_exact_layout(&expected)?;

    let mut seen = BTreeSet::new();
    let mut counters = Counters::default();
    validate_directory(
        &root,
        Path::new(""),
        production,
        &expected,
        &mut seen,
        &mut counters,
    )?;
    ensure!(
        seen.len() == expected.len(),
        "release is missing one or more manifest entries"
    );

    let lock = read_small_regular_file(
        &root.join("config/mediamtx.lock"),
        0o444,
        1024 * 1024,
        production,
        "MediaMTX lock",
    )?;
    ensure!(
        lock == MEDIAMTX_LOCK,
        "release MediaMTX lock differs from the binary contract"
    );
    let config = read_small_regular_file(
        &root.join("config/mediamtx.yml"),
        0o444,
        1024 * 1024,
        production,
        "MediaMTX configuration",
    )?;
    ensure!(
        sha256_hex(&config) == identity.mediamtx_config_sha256,
        "release MediaMTX configuration differs from the binary contract"
    );
    let Some(ManifestEntry::File { sha256, .. }) = expected.get("bin/mediamtx") else {
        bail!("release manifest omits the MediaMTX binary");
    };
    ensure!(
        sha256 == &identity.mediamtx_sha256,
        "release MediaMTX binary differs from the pinned companion contract"
    );
    crate::static_assets::validate(&root.join("web"), true)
        .context("release Web tree differs from the binary static contract")?;

    Ok(identity)
}

fn validate_release_root(root: &Path) -> Result<PathBuf> {
    ensure!(root.is_absolute(), "RELEASE_ROOT must be absolute");
    ensure!(
        root.components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_))),
        "RELEASE_ROOT must be a normalized absolute path"
    );
    let named = fs::symlink_metadata(root).context("inspect RELEASE_ROOT")?;
    ensure!(
        named.is_dir() && !named.file_type().is_symlink(),
        "RELEASE_ROOT must be a real directory"
    );
    let canonical = fs::canonicalize(root).context("resolve RELEASE_ROOT")?;
    ensure!(
        canonical == root,
        "RELEASE_ROOT and every parent component must be physical and normalized"
    );
    ensure!(
        canonical.ends_with(RELOCATABLE_RELEASE_SUFFIX),
        "RELEASE_ROOT must end in the fixed Sentinel 0.2.0 physical release path"
    );
    Ok(canonical)
}

fn parse_entries(records: &str) -> Result<BTreeMap<String, ManifestEntry>> {
    ensure!(
        !records.is_empty(),
        "release manifest has no payload records"
    );
    ensure!(
        records.ends_with('\n'),
        "release manifest must end in a newline"
    );
    let mut entries = BTreeMap::new();
    let mut previous: Option<&str> = None;
    for line in records.lines() {
        ensure!(!line.is_empty(), "release manifest contains a blank line");
        ensure!(
            entries.len() < MAX_ENTRIES,
            "release manifest exceeds the entry limit"
        );
        let fields: Vec<&str> = line.split(' ').collect();
        let (path, entry) = match fields.as_slice() {
            ["directory", mode, path] => {
                validate_relative_path(path)?;
                (
                    *path,
                    ManifestEntry::Directory {
                        mode: parse_mode(mode)?,
                    },
                )
            }
            ["file", mode, size, sha256, path] => {
                validate_relative_path(path)?;
                validate_sha256(sha256)?;
                let size = size.parse::<u64>().context("invalid release file size")?;
                ensure!(
                    size <= MAX_FILE_BYTES,
                    "release file exceeds its size limit"
                );
                (
                    *path,
                    ManifestEntry::File {
                        mode: parse_mode(mode)?,
                        size,
                        sha256: (*sha256).to_owned(),
                    },
                )
            }
            _ => bail!("release manifest contains an invalid or unknown record"),
        };
        ensure!(
            path != MANIFEST_NAME,
            "release manifest must not list itself"
        );
        if let Some(previous) = previous {
            ensure!(
                previous < path,
                "release manifest paths must be strictly sorted"
            );
        }
        previous = Some(path);
        ensure!(
            entries.insert(path.to_owned(), entry).is_none(),
            "release manifest contains a duplicate path"
        );
    }
    Ok(entries)
}

fn validate_exact_layout(entries: &BTreeMap<String, ManifestEntry>) -> Result<()> {
    for directory in FIXED_DIRECTORIES {
        ensure!(
            matches!(
                entries.get(*directory),
                Some(ManifestEntry::Directory { mode: 0o555 })
            ),
            "release manifest is missing required directory {directory}"
        );
    }
    for (file, expected_mode) in FIXED_FILES {
        ensure!(
            matches!(
                entries.get(*file),
                Some(ManifestEntry::File { mode, .. }) if mode == expected_mode
            ),
            "release manifest is missing required file {file}"
        );
    }

    let mut javascript = 0_usize;
    let mut stylesheets = 0_usize;
    for (path, entry) in entries {
        match entry {
            ManifestEntry::Directory { mode } => {
                ensure!(*mode == 0o555, "release directories must have mode 0555");
                ensure!(
                    FIXED_DIRECTORIES.contains(&path.as_str()),
                    "release contains an unexpected directory: {path}"
                );
            }
            ManifestEntry::File { mode, .. } => {
                if let Some((_, expected_mode)) = FIXED_FILES
                    .iter()
                    .find(|(fixed, _)| fixed == &path.as_str())
                {
                    ensure!(
                        mode == expected_mode,
                        "release file has the wrong mode: {path}"
                    );
                    continue;
                }
                ensure!(
                    Path::new(path).parent() == Some(Path::new("web/assets")),
                    "release contains an unexpected file: {path}"
                );
                ensure!(*mode == 0o444, "compiled Web assets must have mode 0444");
                match Path::new(path).extension().and_then(|value| value.to_str()) {
                    Some("js") => javascript += 1,
                    Some("css") => stylesheets += 1,
                    _ => bail!("release contains an unexpected compiled Web asset type: {path}"),
                }
            }
        }
    }
    ensure!(
        javascript > 0,
        "release must contain a compiled JavaScript asset"
    );
    ensure!(stylesheets > 0, "release must contain a compiled CSS asset");
    Ok(())
}

fn validate_directory(
    root: &Path,
    relative: &Path,
    require_root_owned: bool,
    expected: &BTreeMap<String, ManifestEntry>,
    seen: &mut BTreeSet<String>,
    counters: &mut Counters,
) -> Result<()> {
    let directory = root.join(relative);
    let before = fs::symlink_metadata(&directory)?;
    ensure!(
        before.is_dir() && !before.file_type().is_symlink(),
        "release contains an invalid directory"
    );
    if !relative.as_os_str().is_empty() {
        let relative = normalized_relative(relative)?;
        let Some(ManifestEntry::Directory { mode }) = expected.get(&relative) else {
            bail!("release contains an unexpected directory: {relative}");
        };
        require_directory(
            &directory,
            *mode,
            require_root_owned,
            &format!("release directory {relative}"),
        )?;
        ensure!(seen.insert(relative), "duplicate release directory");
    }

    let mut children = fs::read_dir(&directory)?.collect::<std::io::Result<Vec<_>>>()?;
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        let name = child
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("release contains a non-UTF-8 name"))?;
        validate_name(&name)?;
        let child_relative = relative.join(name);
        let child_string = normalized_relative(&child_relative)?;
        if child_string == MANIFEST_NAME {
            continue;
        }
        counters.entries += 1;
        ensure!(
            counters.entries <= MAX_ENTRIES,
            "release contains too many entries"
        );
        let metadata = fs::symlink_metadata(child.path())?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "release contains a symbolic link: {child_string}"
        );
        if metadata.is_dir() {
            validate_directory(
                root,
                &child_relative,
                require_root_owned,
                expected,
                seen,
                counters,
            )?;
            continue;
        }
        ensure!(
            metadata.is_file(),
            "release contains a special file: {child_string}"
        );
        let Some(ManifestEntry::File { mode, size, sha256 }) = expected.get(&child_string) else {
            bail!("release contains an unexpected file: {child_string}");
        };
        let actual = hash_regular_file(
            &child.path(),
            *mode,
            require_root_owned,
            &format!("release file {child_string}"),
        )?;
        ensure!(
            actual.size == *size && actual.sha256 == *sha256,
            "release file size or SHA-256 mismatch: {child_string}"
        );
        counters.bytes = counters
            .bytes
            .checked_add(*size)
            .context("release byte count overflow")?;
        ensure!(
            counters.bytes <= MAX_RELEASE_BYTES,
            "release exceeds its total size limit"
        );
        ensure!(seen.insert(child_string), "duplicate release file");
    }
    let after = fs::symlink_metadata(&directory)?;
    #[cfg(unix)]
    ensure!(
        before.dev() == after.dev() && before.ino() == after.ino(),
        "release directory changed during verification"
    );
    Ok(())
}

fn mediamtx_identity() -> Result<MediaMtxIdentity> {
    let lock = std::str::from_utf8(MEDIAMTX_LOCK).context("embedded MediaMTX lock is not UTF-8")?;
    let values: Vec<&str> = lock
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    ensure!(
        values.len() == 3,
        "embedded MediaMTX lock must contain exactly three fields"
    );
    let version = values[0]
        .strip_prefix("version=")
        .context("embedded MediaMTX lock has no version")?;
    let platform = values[1]
        .strip_prefix("platform=")
        .context("embedded MediaMTX lock has no platform")?;
    let sha256 = values[2]
        .strip_prefix("sha256=")
        .context("embedded MediaMTX lock has no SHA-256")?;
    ensure!(version == "v1.20.0", "unsupported MediaMTX version");
    ensure!(platform == "linux_amd64", "unsupported MediaMTX platform");
    validate_sha256(sha256)?;
    Ok(MediaMtxIdentity {
        version: version.to_owned(),
        platform: platform.to_owned(),
        sha256: sha256.to_owned(),
    })
}

fn hash_regular_file(
    path: &Path,
    mode: u32,
    require_root_owned: bool,
    label: &str,
) -> Result<FileHash> {
    let mut file = open_regular_file(path, mode, require_root_owned, label)?;
    let size = file.metadata()?.len();
    ensure!(size <= MAX_FILE_BYTES, "{label} exceeds its size limit");
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    revalidate_open_file(path, &file, size, label)?;
    Ok(FileHash {
        size,
        sha256: format!("{:x}", hasher.finalize()),
    })
}

fn read_small_regular_file(
    path: &Path,
    mode: u32,
    limit: u64,
    require_root_owned: bool,
    label: &str,
) -> Result<Vec<u8>> {
    let mut file = open_regular_file(path, mode, require_root_owned, label)?;
    let size = file.metadata()?.len();
    ensure!(size <= limit, "{label} exceeds its size limit");
    let mut bytes = Vec::with_capacity(size as usize);
    file.read_to_end(&mut bytes)?;
    revalidate_open_file(path, &file, size, label)?;
    Ok(bytes)
}

fn open_regular_file(
    path: &Path,
    expected_mode: u32,
    require_root_owned: bool,
    label: &str,
) -> Result<File> {
    let named = fs::symlink_metadata(path).with_context(|| format!("inspect {label}"))?;
    ensure!(
        named.is_file() && !named.file_type().is_symlink(),
        "{label} must be a regular non-symlink file"
    );
    #[cfg(unix)]
    {
        ensure!(
            named.nlink() == 1,
            "{label} must not have hard-link aliases"
        );
        ensure!(
            named.permissions().mode() & 0o7777 == expected_mode,
            "{label} has an unexpected mode"
        );
        if require_root_owned {
            ensure!(
                named.uid() == 0 && named.gid() == 0,
                "{label} must be owned by root"
            );
        }
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    let file = options
        .open(path)
        .with_context(|| format!("open {label}"))?;
    revalidate_open_file(path, &file, named.len(), label)?;
    Ok(file)
}

fn revalidate_open_file(path: &Path, file: &File, expected_size: u64, label: &str) -> Result<()> {
    let opened = file.metadata()?;
    let named = fs::symlink_metadata(path)?;
    ensure!(
        opened.is_file()
            && named.is_file()
            && !named.file_type().is_symlink()
            && opened.len() == expected_size
            && named.len() == expected_size,
        "{label} changed while it was verified"
    );
    #[cfg(unix)]
    ensure!(
        opened.dev() == named.dev()
            && opened.ino() == named.ino()
            && opened.nlink() == 1
            && named.nlink() == 1
            && opened.mtime() == named.mtime()
            && opened.mtime_nsec() == named.mtime_nsec(),
        "{label} changed identity while it was verified"
    );
    Ok(())
}

fn require_directory(path: &Path, mode: u32, require_root_owned: bool, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path).with_context(|| format!("inspect {label}"))?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "{label} must be a real directory"
    );
    #[cfg(unix)]
    {
        ensure!(
            metadata.permissions().mode() & 0o7777 == mode,
            "{label} has an unexpected mode"
        );
        if require_root_owned {
            ensure!(
                metadata.uid() == 0 && metadata.gid() == 0,
                "{label} must be owned by root"
            );
        }
    }
    Ok(())
}

fn validate_relative_path(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 1024,
        "invalid release path"
    );
    let path = Path::new(value);
    ensure!(!path.is_absolute(), "release paths must be relative");
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "release path contains an unsafe component"
    );
    for component in path.components() {
        if let Component::Normal(name) = component {
            validate_name(name.to_str().context("release path is not UTF-8")?)?;
        }
    }
    ensure!(
        path.to_str() == Some(value),
        "release path is not canonical"
    );
    Ok(())
}

fn validate_name(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value != "."
            && value != ".."
            && value
                .bytes()
                .all(|byte| { byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-') }),
        "release contains a non-portable name"
    );
    Ok(())
}

fn normalized_relative(path: &Path) -> Result<String> {
    path.to_str()
        .context("release path is not UTF-8")
        .map(|value| value.replace(std::path::MAIN_SEPARATOR, "/"))
}

fn parse_mode(value: &str) -> Result<u32> {
    ensure!(
        value.len() == 3 && value.bytes().all(|byte| matches!(byte, b'0'..=b'7')),
        "release mode must be three octal digits"
    );
    u32::from_str_radix(value, 8).context("release mode is invalid")
}

fn validate_sha256(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
        "SHA-256 values must contain 64 lowercase hexadecimal digits"
    );
    Ok(())
}

fn is_source_revision(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_exact_and_manifest_header_is_canonical() {
        let identity = identity().unwrap();
        assert_eq!(identity.application, PRODUCT);
        assert_eq!(identity.application_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(identity.target, env!("SENTINEL_BUILD_TARGET"));
        assert_eq!(identity.wire_protocol, "sentinel-wire-v2");
        assert_eq!(identity.api_prefix, "/api/v2");
        assert_eq!(identity.schema_revision, 1);
        assert_eq!(identity.credential_envelope_revision, 1);
        assert!(manifest_header().unwrap().ends_with('\n'));
    }

    #[test]
    fn entry_parser_is_strict_and_sorted() {
        let valid = "directory 555 bin\nfile 555 3 abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd bin/app\n";
        assert!(parse_entries(valid).is_ok());
        assert!(parse_entries("directory 755 bin\n").is_ok());
        assert!(parse_entries("directory 555 ../bin\n").is_err());
        assert!(parse_entries("directory 555 z\ndirectory 555 a\n").is_err());
        assert!(parse_entries("compat 555 bin\n").is_err());
        assert!(parse_entries("directory 0555 bin\n").is_err());
    }

    #[test]
    fn source_revision_and_hashes_have_exact_encodings() {
        assert!(is_source_revision(
            "0123456789abcdef0123456789abcdef01234567"
        ));
        assert!(!is_source_revision("v0.2.0"));
        assert!(validate_sha256(&"a".repeat(64)).is_ok());
        assert!(validate_sha256(&"A".repeat(64)).is_err());
    }
}
