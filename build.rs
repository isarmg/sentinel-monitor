use std::{env, fs, path::PathBuf};

const UNBOUND_MANIFEST: &str = "format=sentinel-static-layout-v1\napplication=sentinel-monitor\napplication_version=0.2.0\nunbound=true\n";

fn main() {
    println!("cargo:rerun-if-env-changed=SENTINEL_STATIC_MANIFEST_PATH");

    let manifest = match env::var_os("SENTINEL_STATIC_MANIFEST_PATH") {
        Some(path) => {
            let path = PathBuf::from(path);
            assert!(
                path.is_absolute(),
                "SENTINEL_STATIC_MANIFEST_PATH must be absolute"
            );
            println!("cargo:rerun-if-changed={}", path.display());
            let manifest = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("failed to read static manifest: {error}"));
            assert!(
                manifest.len() <= 1024 * 1024,
                "static manifest exceeds the 1 MiB build limit"
            );
            manifest
        }
        None => UNBOUND_MANIFEST.to_string(),
    };

    let output = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"))
        .join("sentinel-static-layout.manifest");
    fs::write(output, manifest).expect("write embedded static manifest");
}
