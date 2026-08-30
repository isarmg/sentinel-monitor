use anyhow::{bail, ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
};

const EMBEDDED_MANIFEST: &str =
    include_str!(concat!(env!("OUT_DIR"), "/sentinel-static-layout.manifest"));
const FORMAT: &str = "sentinel-static-layout-v1";
const APPLICATION: &str = "sentinel-monitor";
const MAX_FILES: usize = 10_000;
const MAX_ASSET_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
enum ExpectedEntry {
    Directory,
    File { size: u64, sha256: [u8; 32] },
}

pub fn embedded_contract_sha256() -> Result<String> {
    parse_manifest(EMBEDDED_MANIFEST)
        .context("binary was not built with a valid static asset contract")?;
    Ok(encode_hex(&Sha256::digest(EMBEDDED_MANIFEST.as_bytes())))
}

pub fn validate(static_dir: &Path, production: bool) -> Result<()> {
    validate_with_manifest(static_dir, production, EMBEDDED_MANIFEST)
}

fn validate_with_manifest(static_dir: &Path, production: bool, manifest: &str) -> Result<()> {
    ensure!(
        static_dir.is_absolute(),
        "STATIC_DIR must be an absolute path"
    );
    validate_directory_chain(static_dir)?;
    let expected = parse_manifest(manifest)?;
    let mut seen = HashSet::with_capacity(expected.len());
    validate_tree(static_dir, Path::new(""), production, &expected, &mut seen)?;
    ensure!(
        seen.len() == expected.len(),
        "STATIC_DIR is missing one or more files from this binary's static contract"
    );
    Ok(())
}

fn parse_manifest(manifest: &str) -> Result<HashMap<PathBuf, ExpectedEntry>> {
    ensure!(
        manifest.len() <= 1024 * 1024,
        "embedded static manifest exceeds the 1 MiB limit"
    );
    let mut lines = manifest.lines();
    let expected_format = format!("format={FORMAT}");
    ensure!(
        lines.next() == Some(expected_format.as_str()),
        "binary has an invalid static manifest format"
    );
    let expected_application = format!("application={APPLICATION}");
    ensure!(
        lines.next() == Some(expected_application.as_str()),
        "binary static manifest belongs to a different application"
    );
    let expected_version = format!("application_version={}", env!("CARGO_PKG_VERSION"));
    ensure!(
        lines.next() == Some(expected_version.as_str()),
        "binary static manifest belongs to a different application version"
    );

    let mut entries = HashMap::new();
    for line in lines {
        ensure!(!line.is_empty(), "static manifest contains a blank line");
        ensure!(
            entries.len() < MAX_FILES,
            "static manifest exceeds the entry limit"
        );
        if let Some(path) = line.strip_prefix("directory ") {
            let path = validated_relative_path(path)?;
            ensure!(
                entries.insert(path, ExpectedEntry::Directory).is_none(),
                "static manifest contains a duplicate path"
            );
            continue;
        }
        let Some(rest) = line.strip_prefix("file ") else {
            bail!("static manifest contains an unknown record");
        };
        let mut fields = rest.splitn(3, ' ');
        let size = fields
            .next()
            .context("static manifest file record is missing its size")?
            .parse::<u64>()
            .context("static manifest file size is invalid")?;
        ensure!(
            size <= MAX_ASSET_BYTES,
            "static asset exceeds the size limit"
        );
        let digest = fields
            .next()
            .context("static manifest file record is missing its digest")?;
        let sha256 = decode_digest(digest)?;
        let path = validated_relative_path(
            fields
                .next()
                .context("static manifest file record is missing its path")?,
        )?;
        ensure!(
            entries
                .insert(path, ExpectedEntry::File { size, sha256 })
                .is_none(),
            "static manifest contains a duplicate path"
        );
    }

    ensure!(
        matches!(
            entries.get(Path::new("index.html")),
            Some(ExpectedEntry::File { .. })
        ),
        "static manifest does not contain index.html"
    );
    ensure!(
        entries.iter().any(|(path, entry)| {
            matches!(entry, ExpectedEntry::File { .. })
                && path.extension().and_then(|value| value.to_str()) == Some("js")
        }),
        "static manifest does not contain a JavaScript asset"
    );
    ensure!(
        entries.iter().any(|(path, entry)| {
            matches!(entry, ExpectedEntry::File { .. })
                && path.extension().and_then(|value| value.to_str()) == Some("css")
        }),
        "static manifest does not contain a CSS asset"
    );
    for path in entries.keys() {
        let mut parent = path.parent();
        while let Some(directory) = parent.filter(|value| !value.as_os_str().is_empty()) {
            ensure!(
                entries.get(directory) == Some(&ExpectedEntry::Directory),
                "static manifest omits a parent directory"
            );
            parent = directory.parent();
        }
    }
    Ok(entries)
}

fn validate_directory_chain(path: &Path) -> Result<()> {
    let mut current = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir => continue,
            Component::Normal(name) => current.push(name),
            _ => bail!("STATIC_DIR contains an invalid path component"),
        }
        let metadata = fs::symlink_metadata(&current).with_context(|| {
            format!("STATIC_DIR component is unavailable: {}", current.display())
        })?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "STATIC_DIR must not traverse symbolic links"
        );
        ensure!(
            metadata.is_dir(),
            "STATIC_DIR components must be directories"
        );
    }
    Ok(())
}

fn validate_tree(
    root: &Path,
    relative: &Path,
    production: bool,
    expected: &HashMap<PathBuf, ExpectedEntry>,
    seen: &mut HashSet<PathBuf>,
) -> Result<()> {
    let path = root.join(relative);
    let before = fs::symlink_metadata(&path)
        .with_context(|| format!("failed to inspect static path: {}", relative.display()))?;
    ensure!(
        !before.file_type().is_symlink(),
        "static layout contains a symbolic link"
    );
    ensure!(
        before.is_dir(),
        "static layout contains a non-directory where a directory is required"
    );
    if production {
        ensure_read_only(&before, "static directory")?;
    }
    if !relative.as_os_str().is_empty() {
        ensure!(
            expected.get(relative) == Some(&ExpectedEntry::Directory),
            "STATIC_DIR contains an unexpected directory"
        );
        ensure!(
            seen.insert(relative.to_path_buf()),
            "STATIC_DIR contains a duplicate path"
        );
    }

    let mut children = fs::read_dir(&path)
        .with_context(|| format!("failed to read static directory: {}", relative.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        let name = child
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("static layout contains a non-UTF-8 name"))?;
        validate_name(&name)?;
        let child_relative = relative.join(name);
        let metadata = fs::symlink_metadata(child.path())?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "static layout contains a symbolic link"
        );
        if metadata.is_dir() {
            validate_tree(root, &child_relative, production, expected, seen)?;
        } else if metadata.is_file() {
            let Some(ExpectedEntry::File { size, sha256 }) = expected.get(&child_relative) else {
                bail!("STATIC_DIR contains an unexpected file");
            };
            validate_file(&child.path(), &metadata, *size, sha256, production)?;
            ensure!(
                seen.insert(child_relative),
                "STATIC_DIR contains a duplicate path"
            );
        } else {
            bail!("static layout contains a special file");
        }
    }

    let after = fs::symlink_metadata(&path)?;
    ensure!(
        before.dev() == after.dev() && before.ino() == after.ino(),
        "static directory changed while it was being validated"
    );
    Ok(())
}

fn validate_file(
    path: &Path,
    path_metadata: &fs::Metadata,
    expected_size: u64,
    expected_sha256: &[u8; 32],
    production: bool,
) -> Result<()> {
    ensure!(
        path_metadata.nlink() == 1,
        "static assets must not be hard linked"
    );
    ensure!(
        path_metadata.len() == expected_size,
        "static asset size does not match this binary"
    );
    if production {
        ensure_read_only(path_metadata, "static asset")?;
    }

    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .context("failed to open a static asset without following links")?;
    let opened = file.metadata()?;
    ensure!(opened.is_file(), "static asset is not a regular file");
    ensure!(opened.nlink() == 1, "static assets must not be hard linked");
    ensure!(
        opened.dev() == path_metadata.dev() && opened.ino() == path_metadata.ino(),
        "static asset changed while it was being opened"
    );
    let mut digest = Sha256::new();
    let copied = std::io::copy(&mut file, &mut digest)?;
    ensure!(
        copied == expected_size,
        "static asset changed while it was being read"
    );
    let actual: [u8; 32] = digest.finalize().into();
    ensure!(
        &actual == expected_sha256,
        "static asset digest does not match this binary"
    );
    let after = file.metadata()?;
    ensure!(
        opened.dev() == after.dev()
            && opened.ino() == after.ino()
            && after.nlink() == 1
            && after.len() == expected_size,
        "static asset changed while it was being validated"
    );
    Ok(())
}

fn ensure_read_only(metadata: &fs::Metadata, kind: &str) -> Result<()> {
    ensure!(
        metadata.permissions().mode() & 0o222 == 0,
        "production {kind} must not have writable permission bits"
    );
    Ok(())
}

fn validated_relative_path(value: &str) -> Result<PathBuf> {
    ensure!(!value.is_empty(), "static manifest path is empty");
    ensure!(value.len() <= 1024, "static manifest path is too long");
    let path = PathBuf::from(value);
    ensure!(!path.is_absolute(), "static manifest path must be relative");
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "static manifest path contains an unsafe component"
    );
    for component in path.components() {
        if let Component::Normal(name) = component {
            validate_name(
                name.to_str()
                    .context("static manifest path is not valid UTF-8")?,
            )?;
        }
    }
    Ok(path)
}

fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')),
        "static layout contains a non-portable name"
    );
    ensure!(
        name != "." && name != "..",
        "static layout contains an unsafe name"
    );
    Ok(())
}

fn decode_digest(value: &str) -> Result<[u8; 32]> {
    ensure!(
        value.len() == 64,
        "static manifest digest must contain 64 hex digits"
    );
    let mut output = [0_u8; 32];
    for (index, byte) in output.iter_mut().enumerate() {
        let offset = index * 2;
        *byte = (hex_nibble(value.as_bytes()[offset])? << 4)
            | hex_nibble(value.as_bytes()[offset + 1])?;
    }
    Ok(output)
}

fn hex_nibble(value: u8) -> Result<u8> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => bail!("static manifest digest must use lowercase hexadecimal"),
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use tempfile::TempDir;

    struct StaticFixture {
        temp: TempDir,
        root: PathBuf,
    }

    impl StaticFixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("release/web");
            fs::create_dir_all(root.join("assets")).unwrap();
            fs::write(
                root.join("index.html"),
                b"<script src=/assets/app.js></script>",
            )
            .unwrap();
            fs::write(root.join("assets/app.js"), b"console.log('sentinel')").unwrap();
            fs::write(root.join("assets/app.css"), b"body{color:#123}").unwrap();
            Self { temp, root }
        }

        fn manifest(&self) -> String {
            let mut output = format!(
                "format={FORMAT}\napplication={APPLICATION}\napplication_version={}\n",
                env!("CARGO_PKG_VERSION")
            );
            output.push_str("directory assets\n");
            for relative in ["assets/app.css", "assets/app.js", "index.html"] {
                let bytes = fs::read(self.root.join(relative)).unwrap();
                output.push_str(&format!(
                    "file {} {} {}\n",
                    bytes.len(),
                    encode_hex(&Sha256::digest(&bytes)),
                    relative
                ));
            }
            output
        }

        fn make_read_only(&self) {
            for path in [
                self.root.join("index.html"),
                self.root.join("assets/app.js"),
                self.root.join("assets/app.css"),
            ] {
                fs::set_permissions(path, fs::Permissions::from_mode(0o444)).unwrap();
            }
            fs::set_permissions(self.root.join("assets"), fs::Permissions::from_mode(0o555))
                .unwrap();
            fs::set_permissions(&self.root, fs::Permissions::from_mode(0o555)).unwrap();
        }
    }

    impl Drop for StaticFixture {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.root, fs::Permissions::from_mode(0o755));
            let _ =
                fs::set_permissions(self.root.join("assets"), fs::Permissions::from_mode(0o755));
            let _ = &self.temp;
        }
    }

    #[test]
    fn exact_read_only_static_layout_is_accepted() {
        let fixture = StaticFixture::new();
        let manifest = fixture.manifest();
        fixture.make_read_only();
        validate_with_manifest(&fixture.root, true, &manifest).unwrap();
    }

    #[test]
    fn changed_missing_and_extra_assets_are_rejected() {
        let fixture = StaticFixture::new();
        let manifest = fixture.manifest();
        fs::write(fixture.root.join("assets/app.js"), b"changed").unwrap();
        assert!(validate_with_manifest(&fixture.root, false, &manifest).is_err());
        fs::write(
            fixture.root.join("assets/app.js"),
            b"console.log('sentinel')",
        )
        .unwrap();
        fs::remove_file(fixture.root.join("assets/app.css")).unwrap();
        assert!(validate_with_manifest(&fixture.root, false, &manifest).is_err());
        fs::write(fixture.root.join("assets/app.css"), b"body{color:#123}").unwrap();
        fs::write(fixture.root.join("extra.txt"), b"extra").unwrap();
        assert!(validate_with_manifest(&fixture.root, false, &manifest).is_err());
    }

    #[test]
    fn symlinks_and_hard_links_are_rejected() {
        let fixture = StaticFixture::new();
        let manifest = fixture.manifest();
        fs::remove_file(fixture.root.join("assets/app.js")).unwrap();
        symlink("app.css", fixture.root.join("assets/app.js")).unwrap();
        assert!(validate_with_manifest(&fixture.root, false, &manifest).is_err());

        fs::remove_file(fixture.root.join("assets/app.js")).unwrap();
        fs::write(
            fixture.root.join("assets/app.js"),
            b"console.log('sentinel')",
        )
        .unwrap();
        fs::hard_link(
            fixture.root.join("assets/app.js"),
            fixture.temp.path().join("asset-hardlink-alias"),
        )
        .unwrap();
        assert!(validate_with_manifest(&fixture.root, false, &manifest).is_err());
    }

    #[test]
    fn an_intermediate_directory_symlink_is_rejected() {
        let fixture = StaticFixture::new();
        let manifest = fixture.manifest();
        let alias = fixture.temp.path().join("release-alias");
        symlink(fixture.temp.path().join("release"), &alias).unwrap();
        assert!(validate_with_manifest(&alias.join("web"), false, &manifest).is_err());
    }

    #[test]
    fn writable_assets_are_only_allowed_in_development() {
        let fixture = StaticFixture::new();
        let manifest = fixture.manifest();
        validate_with_manifest(&fixture.root, false, &manifest).unwrap();

        fixture.make_read_only();
        fs::set_permissions(
            fixture.root.join("assets/app.js"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(validate_with_manifest(&fixture.root, true, &manifest).is_err());

        fs::set_permissions(
            fixture.root.join("assets/app.js"),
            fs::Permissions::from_mode(0o444),
        )
        .unwrap();
        fs::set_permissions(
            fixture.root.join("assets"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(validate_with_manifest(&fixture.root, true, &manifest).is_err());
    }

    #[test]
    fn special_files_are_rejected() {
        let fixture = StaticFixture::new();
        let manifest = fixture.manifest();
        let fifo = fixture.root.join("asset.pipe");
        let fifo = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `fifo` is a live, NUL-terminated path and the mode is valid.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let error = validate_with_manifest(&fixture.root, false, &manifest).unwrap_err();
        assert!(format!("{error:#}").contains("special file"));
    }
}
