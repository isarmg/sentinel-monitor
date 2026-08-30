#!/usr/bin/env bash
set -euo pipefail

SOURCE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
# shellcheck source=/dev/null
source "$SOURCE_ROOT/native/common.sh"

INSTALL_ROOT="${SENTINEL_NATIVE_INSTALL_ROOT:-/opt/isarmg/sentinel-monitor}"
BUILD_TARGET="${SENTINEL_BUILD_TARGET:-/var/tmp/sentinel-monitor-build}"
MEDIA_SOURCE="${SENTINEL_MEDIAMTX_SOURCE:-}"
APP_SOURCE="${SENTINEL_APP_BINARY_SOURCE:-}"
WEB_SOURCE="${SENTINEL_WEB_SOURCE:-}"

validate_absolute_path "$INSTALL_ROOT" "SENTINEL_NATIVE_INSTALL_ROOT"
validate_absolute_path "$BUILD_TARGET" "SENTINEL_BUILD_TARGET"
[[ -n "$MEDIA_SOURCE" ]] || die "SENTINEL_MEDIAMTX_SOURCE is required"
validate_absolute_path "$MEDIA_SOURCE" "SENTINEL_MEDIAMTX_SOURCE"
assert_regular_file "$MEDIA_SOURCE" "MediaMTX source binary"

# Prebuilt inputs exist only to keep the temporary-root lifecycle test fast.
# They can never publish under a production path.
if [[ -n "$APP_SOURCE" || -n "$WEB_SOURCE" ]]; then
  FIXTURE_ROOT="${SENTINEL_NATIVE_TEST_ROOT:-}"
  [[ -n "$FIXTURE_ROOT" ]] ||
    die "prebuilt application/Web inputs are restricted to the native lifecycle test"
  validate_absolute_path "$FIXTURE_ROOT" "SENTINEL_NATIVE_TEST_ROOT"
  assert_private_directory "$FIXTURE_ROOT" "native lifecycle test root"
  for fixture_path in "$SOURCE_ROOT" "$INSTALL_ROOT" "$BUILD_TARGET" "$MEDIA_SOURCE" "$APP_SOURCE" "$WEB_SOURCE"; do
    [[ -z "$fixture_path" || "$fixture_path" == "$FIXTURE_ROOT/"* ]] ||
      die "native lifecycle fixture paths must remain below SENTINEL_NATIVE_TEST_ROOT"
  done
fi
require_command awk
require_command cmp
require_command find
require_command flock
require_command sha256sum
require_command sort

SOURCE_VERSION="$(awk '
  /^\[package\]$/ { package = 1; next }
  /^\[/ { package = 0 }
  package && /^version[[:space:]]*=/ {
    value = $0
    sub(/^[^=]*=[[:space:]]*"/, "", value)
    sub(/".*/, "", value)
    print value
    exit
  }
' "$SOURCE_ROOT/Cargo.toml")"
[[ "$SOURCE_VERSION" == "$SENTINEL_VERSION" ]] ||
  die "Cargo package version must be exactly $SENTINEL_VERSION"

manifest_value() {
  local key="$1"
  awk -F= -v key="$key" '$1 == key { print $2 }' "$SOURCE_ROOT/native/mediamtx.lock"
}

EXPECTED_MEDIA_VERSION="$(manifest_value version)"
EXPECTED_MEDIA_PLATFORM="$(manifest_value platform)"
EXPECTED_MEDIA_SHA256="$(manifest_value sha256)"
[[ "$EXPECTED_MEDIA_PLATFORM" == "linux_amd64" ]] ||
  die "Unsupported MediaMTX companion platform: $EXPECTED_MEDIA_PLATFORM"
[[ "$($MEDIA_SOURCE --version | tr -d '\r\n')" == "$EXPECTED_MEDIA_VERSION" ]] ||
  die "MediaMTX source version does not match native/mediamtx.lock"
[[ "$(sha256sum -- "$MEDIA_SOURCE" | awk '{print $1}')" == "$EXPECTED_MEDIA_SHA256" ]] ||
  die "MediaMTX source SHA-256 does not match native/mediamtx.lock"

ensure_directory "$BUILD_TARGET" 755 "build target"
TEMPORARY="$(mktemp -d "$BUILD_TARGET/release-build.XXXXXX")"
STAGE=""
CURRENT_TEMP=""
cleanup() {
  if [[ -n "$STAGE" && -d "$STAGE" ]]; then
    chmod -R u+w -- "$STAGE" 2>/dev/null || true
    rm -rf -- "$STAGE"
  fi
  if [[ -n "$CURRENT_TEMP" && -L "$CURRENT_TEMP" ]]; then
    rm -- "$CURRENT_TEMP"
  fi
  chmod -R u+w -- "$TEMPORARY" 2>/dev/null || true
  rm -rf -- "$TEMPORARY"
}
trap cleanup EXIT

WEB_STAGE="$TEMPORARY/web"
mkdir -- "$WEB_STAGE"
if [[ -n "$WEB_SOURCE" ]]; then
  validate_absolute_path "$WEB_SOURCE" "SENTINEL_WEB_SOURCE"
  assert_directory "$WEB_SOURCE" "prebuilt Web source"
  cp -a -- "$WEB_SOURCE/." "$WEB_STAGE/"
else
  require_command npm
  npm ci --prefix "$SOURCE_ROOT/web"
  npm run build --prefix "$SOURCE_ROOT/web" -- --outDir "$WEB_STAGE" --emptyOutDir
fi
find -P "$WEB_STAGE" -type d -exec chmod 0555 -- {} +
find -P "$WEB_STAGE" -type f -exec chmod 0444 -- {} +
STATIC_MANIFEST="$TEMPORARY/static-layout.manifest"
write_static_manifest "$WEB_STAGE" "$STATIC_MANIFEST"
STATIC_CONTRACT="$(sha256sum -- "$STATIC_MANIFEST" | awk '{print $1}')"

APP_STAGE="$TEMPORARY/sentinel-monitor"
if [[ -n "$APP_SOURCE" ]]; then
  validate_absolute_path "$APP_SOURCE" "SENTINEL_APP_BINARY_SOURCE"
  assert_regular_file "$APP_SOURCE" "prebuilt Sentinel binary"
  install -m 0755 -- "$APP_SOURCE" "$APP_STAGE"
else
  require_command cargo
  CARGO_TARGET_DIR="$BUILD_TARGET/cargo" \
    SENTINEL_STATIC_MANIFEST_PATH="$STATIC_MANIFEST" \
    cargo build --locked --release --manifest-path "$SOURCE_ROOT/Cargo.toml"
  install -m 0755 -- "$BUILD_TARGET/cargo/release/sentinel-monitor" "$APP_STAGE"
fi
[[ "$($APP_STAGE --version | tr -d '\r\n')" == "$SENTINEL_PRODUCT $SENTINEL_VERSION" ]] ||
  die "Sentinel application binary version is not $SENTINEL_VERSION"
[[ "$($APP_STAGE static-contract | tr -d '\r\n')" == "$STATIC_CONTRACT" ]] ||
  die "Sentinel binary is not bound to the staged Web asset contract"

ensure_directory "$INSTALL_ROOT" 755 "install root"
RELEASES_ROOT="$INSTALL_ROOT/releases"
ensure_directory "$RELEASES_ROOT" 755 "releases directory"
STAGE="$(mktemp -d "$RELEASES_ROOT/.${SENTINEL_VERSION}.stage.XXXXXX")"
mkdir -p -- "$STAGE/bin" "$STAGE/config" "$STAGE/native" "$STAGE/web"
install -m 0555 -- "$APP_STAGE" "$STAGE/bin/sentinel-monitor"
install -m 0555 -- "$MEDIA_SOURCE" "$STAGE/bin/mediamtx"
install -m 0444 -- "$SOURCE_ROOT/native/mediamtx.yml" "$STAGE/config/mediamtx.yml"
install -m 0444 -- "$SOURCE_ROOT/native/mediamtx.lock" "$STAGE/config/mediamtx.lock"
for script in common.sh bootstrap.sh start.sh status.sh stop.sh; do
  install -m 0555 -- "$SOURCE_ROOT/native/$script" "$STAGE/native/$script"
done
cp -a -- "$WEB_STAGE/." "$STAGE/web/"
find -P "$STAGE/web" -type d -exec chmod 0555 -- {} +
find -P "$STAGE/web" -type f -exec chmod 0444 -- {} +
find -P "$STAGE" -mindepth 1 -type d -exec chmod 0555 -- {} +

# Revalidate the bytes that will actually be published, closing mutation
# windows between source validation and installation into the release stage.
[[ "$("$STAGE/bin/mediamtx" --version | tr -d '\r\n')" == "$EXPECTED_MEDIA_VERSION" ]] ||
  die "Staged MediaMTX version does not match the pinned contract"
[[ "$(sha256sum -- "$STAGE/bin/mediamtx" | awk '{print $1}')" == "$EXPECTED_MEDIA_SHA256" ]] ||
  die "Staged MediaMTX SHA-256 does not match the pinned contract"
[[ "$(awk -F= '$1 == "version" { print $2 }' "$STAGE/config/mediamtx.lock")" == "$EXPECTED_MEDIA_VERSION" ]] ||
  die "Staged MediaMTX lock changed during publication"
[[ "$(awk -F= '$1 == "platform" { print $2 }' "$STAGE/config/mediamtx.lock")" == "$EXPECTED_MEDIA_PLATFORM" ]] ||
  die "Staged MediaMTX platform lock changed during publication"
[[ "$(awk -F= '$1 == "sha256" { print $2 }' "$STAGE/config/mediamtx.lock")" == "$EXPECTED_MEDIA_SHA256" ]] ||
  die "Staged MediaMTX digest lock changed during publication"
STAGED_STATIC_MANIFEST="$TEMPORARY/staged-static-layout.manifest"
write_static_manifest "$STAGE/web" "$STAGED_STATIC_MANIFEST"
STAGED_STATIC_CONTRACT="$(sha256sum -- "$STAGED_STATIC_MANIFEST" | awk '{print $1}')"
[[ "$("$STAGE/bin/sentinel-monitor" --version | tr -d '\r\n')" == "$SENTINEL_PRODUCT $SENTINEL_VERSION" ]] ||
  die "Staged Sentinel application has the wrong version"
[[ "$("$STAGE/bin/sentinel-monitor" static-contract | tr -d '\r\n')" == "$STAGED_STATIC_CONTRACT" ]] ||
  die "Staged Sentinel application is not bound to the staged Web assets"

RELEASE_MANIFEST="$TEMPORARY/RELEASE-MANIFEST"
write_release_manifest "$STAGE" "$RELEASE_MANIFEST"
install -m 0444 -- "$RELEASE_MANIFEST" "$STAGE/RELEASE-MANIFEST"
chmod 0555 -- "$STAGE"
verify_release "$STAGE"

RELEASE_ROOT="$RELEASES_ROOT/$SENTINEL_VERSION"
if [[ -e "$RELEASE_ROOT" || -L "$RELEASE_ROOT" ]]; then
  [[ ! -L "$RELEASE_ROOT" && -d "$RELEASE_ROOT" ]] ||
    die "Existing release target is not a real directory: $RELEASE_ROOT"
  verify_release "$RELEASE_ROOT"
  if ! cmp -s -- "$STAGE/RELEASE-MANIFEST" "$RELEASE_ROOT/RELEASE-MANIFEST"; then
    die "Release $SENTINEL_VERSION already exists with different content"
  fi
  chmod -R u+w -- "$STAGE"
  rm -rf -- "$STAGE"
  STAGE=""
else
  mv -T -- "$STAGE" "$RELEASE_ROOT"
  STAGE=""
fi

CURRENT="$INSTALL_ROOT/current"
if [[ -e "$CURRENT" && ! -L "$CURRENT" ]]; then
  die "current must be absent or a managed symbolic link"
fi
CURRENT_TEMP="$INSTALL_ROOT/.current.${SENTINEL_VERSION}.$$"
[[ ! -e "$CURRENT_TEMP" && ! -L "$CURRENT_TEMP" ]] || die "Temporary current link already exists"
ln -s -- "releases/$SENTINEL_VERSION" "$CURRENT_TEMP"
mv -Tf -- "$CURRENT_TEMP" "$CURRENT"
CURRENT_TEMP=""

echo "Published immutable Sentinel release: $RELEASE_ROOT"
echo "Next: $CURRENT/native/bootstrap.sh"
