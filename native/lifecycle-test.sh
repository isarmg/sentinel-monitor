#!/usr/bin/env bash
set -euo pipefail

REPOSITORY_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
# shellcheck source=/dev/null
source "$REPOSITORY_ROOT/native/common.sh"

TEST_ROOT="$(mktemp -d)"
SOURCE_FIXTURE="$TEST_ROOT/source"
INSTALL_ROOT="$TEST_ROOT/opt/isarmg/sentinel-monitor"
CONFIG_ROOT="$TEST_ROOT/etc/isarmg"
STATE_ROOT="$TEST_ROOT/var/lib/isarmg/sentinel-monitor"
RUNTIME_ROOT="$TEST_ROOT/run/isarmg/sentinel-monitor"
BUILD_ROOT="$TEST_ROOT/build"
FAKE_BIN="$TEST_ROOT/test-bin"
OPERATION_LOCK_PID=""

cleanup() {
  if [[ -n "$OPERATION_LOCK_PID" ]]; then
    kill "$OPERATION_LOCK_PID" 2>/dev/null || true
    wait "$OPERATION_LOCK_PID" 2>/dev/null || true
  fi
  for pid_file in "$RUNTIME_ROOT/app.pid" "$RUNTIME_ROOT/mediamtx.pid"; do
    if [[ -f "$pid_file" ]]; then
      pid="$(<"$pid_file")"
      if [[ "$pid" =~ ^[1-9][0-9]*$ ]]; then
        kill "$pid" 2>/dev/null || true
      fi
    fi
  done
  chmod -R u+w -- "$TEST_ROOT" 2>/dev/null || true
  rm -rf -- "$TEST_ROOT"
}
trap cleanup EXIT

fail() {
  echo "lifecycle test failed: $*" >&2
  exit 1
}

mkdir -p -- "$SOURCE_FIXTURE/native" "$SOURCE_FIXTURE/config" "$SOURCE_FIXTURE/web-dist/assets" "$FAKE_BIN"
install -m 0755 -- \
  "$REPOSITORY_ROOT/native/common.sh" \
  "$REPOSITORY_ROOT/native/build.sh" \
  "$REPOSITORY_ROOT/native/bootstrap.sh" \
  "$REPOSITORY_ROOT/native/start.sh" \
  "$REPOSITORY_ROOT/native/status.sh" \
  "$REPOSITORY_ROOT/native/stop.sh" \
  "$SOURCE_FIXTURE/native/"
install -m 0644 -- "$REPOSITORY_ROOT/config/mediamtx.yml" "$SOURCE_FIXTURE/config/mediamtx.yml"
printf '%s\n' \
  '[package]' \
  'name = "sentinel-monitor"' \
  'version = "0.2.0"' \
  >"$SOURCE_FIXTURE/Cargo.toml"
printf '%s\n' '<!doctype html><script type="module" src="/assets/app.js"></script><link rel="stylesheet" href="/assets/app.css">' \
  >"$SOURCE_FIXTURE/web-dist/index.html"
printf '%s\n' 'console.log("sentinel lifecycle")' >"$SOURCE_FIXTURE/web-dist/assets/app.js"
printf '%s\n' 'body { color: #123456; }' >"$SOURCE_FIXTURE/web-dist/assets/app.css"

FAKE_MEDIA="$TEST_ROOT/fake-mediamtx"
cat >"$FAKE_MEDIA" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "--version" ]]; then
  echo 'v1.20.0'
  exit 0
fi
if [[ -n "${FAKE_MEDIA_PID_AUDIT:-}" ]]; then
  printf '%s\n' "$$" >"$FAKE_MEDIA_PID_AUDIT"
fi
trap 'exit 0' TERM INT
while :; do sleep 1; done
EOF
chmod 0755 -- "$FAKE_MEDIA"
FAKE_MEDIA_SHA="$(sha256sum -- "$FAKE_MEDIA" | awk '{print $1}')"
FAKE_SOURCE_REVISION="0123456789abcdef0123456789abcdef01234567"
FAKE_CONFIG_SHA="$(sha256sum -- "$SOURCE_FIXTURE/config/mediamtx.yml" | awk '{print $1}')"
FAKE_RELEASE_CONTRACT="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
printf '%s\n' \
  '# Sentinel lifecycle fixture companion contract.' \
  'version=v1.20.0' \
  'platform=linux_amd64' \
  "sha256=$FAKE_MEDIA_SHA" \
  >"$SOURCE_FIXTURE/config/mediamtx.lock"

FAKE_APP="$TEST_ROOT/fake-sentinel-monitor"
cat >"$FAKE_APP" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
case "${1:-serve}" in
  --version)
    echo 'sentinel-monitor 0.2.0'
    ;;
  static-contract)
    printf '%s\n' "${FAKE_STATIC_CONTRACT:?}"
    ;;
  release-manifest-header)
    cat <<HEADER
format=sentinel-release-v2
application=sentinel-monitor
application_version=0.2.0
source_revision=${FAKE_SOURCE_REVISION:?}
target=x86_64-unknown-linux-gnu
wire_protocol=sentinel-wire-v2
api_prefix=/api/v2
schema_revision=3
schema_sha256=18d53d385fda41458b3e614d0f1179409a52137b52c6b69ce5c3c19c5f84506e
credential_envelope_revision=1
credential_contract_sha256=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
static_contract_sha256=${FAKE_STATIC_CONTRACT:?}
mediamtx_version=v1.20.0
mediamtx_platform=linux_amd64
mediamtx_sha256=${FAKE_MEDIA_SHA:?}
mediamtx_config_sha256=${FAKE_CONFIG_SHA:?}
release_contract_sha256=${FAKE_RELEASE_CONTRACT:?}
HEADER
    ;;
  verify-release)
    root="${2:?}"
    [[ "$(readlink -f -- "$0")" == "$(readlink -f -- "$root/bin/sentinel-monitor")" ]]
    ;;
  serve-release)
    root="${2:?}"
    [[ "$(readlink -f -- "$0")" == "$(readlink -f -- "$root/bin/sentinel-monitor")" ]]
    umask 077
    printf '%s\n' "$$" >"${SENTINEL_RUNTIME_DIR:?}/app.pid"
    cleanup() { rm -f -- "$SENTINEL_RUNTIME_DIR/app.pid"; }
    trap cleanup EXIT
    trap 'exit 0' TERM INT
    while :; do sleep 1; done
    ;;
  *)
    exit 2
    ;;
esac
EOF
chmod 0755 -- "$FAKE_APP"

cat >"$FAKE_BIN/curl" <<'EOF'
#!/usr/bin/env bash
if [[ "${SENTINEL_TEST_CURL_FAILURE:-}" == "1" ]]; then
  exit 22
fi
if [[ "${*: -1}" == */readyz ]]; then
  printf '%s' '{"ready":true}:200'
fi
exit 0
EOF
chmod 0755 -- "$FAKE_BIN/curl"

refresh_static_contract() {
  local manifest="$TEST_ROOT/fixture-static.manifest"
  write_static_manifest "$SOURCE_FIXTURE/web-dist" "$manifest"
  FAKE_STATIC_CONTRACT="$(sha256sum -- "$manifest" | awk '{print $1}')"
  export FAKE_STATIC_CONTRACT
}

run_build() {
  env \
    PATH="$FAKE_BIN:$PATH" \
    SENTINEL_NATIVE_INSTALL_ROOT="$INSTALL_ROOT" \
    SENTINEL_NATIVE_TEST_ROOT="$TEST_ROOT" \
    SENTINEL_BUILD_TARGET="$BUILD_ROOT" \
    SENTINEL_MEDIAMTX_SOURCE="$FAKE_MEDIA" \
    SENTINEL_APP_BINARY_SOURCE="$FAKE_APP" \
    SENTINEL_WEB_SOURCE="$SOURCE_FIXTURE/web-dist" \
    SENTINEL_SOURCE_REVISION="$FAKE_SOURCE_REVISION" \
    FAKE_STATIC_CONTRACT="$FAKE_STATIC_CONTRACT" \
    FAKE_SOURCE_REVISION="$FAKE_SOURCE_REVISION" \
    FAKE_MEDIA_SHA="$FAKE_MEDIA_SHA" \
    FAKE_CONFIG_SHA="$FAKE_CONFIG_SHA" \
    FAKE_RELEASE_CONTRACT="$FAKE_RELEASE_CONTRACT" \
    "$SOURCE_FIXTURE/native/build.sh"
}

run_operation() {
  env \
    PATH="$FAKE_BIN:$PATH" \
    SENTINEL_NATIVE_INSTALL_ROOT="$INSTALL_ROOT" \
    SENTINEL_NATIVE_CONFIG_DIR="$CONFIG_ROOT" \
    SENTINEL_NATIVE_STATE_DIR="$STATE_ROOT" \
    SENTINEL_NATIVE_RUNTIME_DIR="$RUNTIME_ROOT" \
    FAKE_STATIC_CONTRACT="$FAKE_STATIC_CONTRACT" \
    FAKE_SOURCE_REVISION="$FAKE_SOURCE_REVISION" \
    FAKE_MEDIA_SHA="$FAKE_MEDIA_SHA" \
    FAKE_CONFIG_SHA="$FAKE_CONFIG_SHA" \
    FAKE_RELEASE_CONTRACT="$FAKE_RELEASE_CONTRACT" \
    "$@"
}

refresh_static_contract
run_build >/dev/null
[[ -d "$INSTALL_ROOT/releases/0.2.0" && ! -L "$INSTALL_ROOT/releases/0.2.0" ]] ||
  fail "physical release directory is missing"
[[ ! -e "$INSTALL_ROOT/current" && ! -L "$INSTALL_ROOT/current" ]] ||
  fail "publisher created a mutable current alias"
[[ -x "$INSTALL_ROOT/releases/0.2.0/native/start.sh" ]] || fail "release operational scripts are missing"
[[ "$(find "$INSTALL_ROOT/releases" -maxdepth 1 -name '.0.2.0.stage.*' -print -quit)" == "" ]] ||
  fail "physical release publication left a staging directory"
[[ -z "$(find -P "$INSTALL_ROOT/releases/0.2.0" -perm /222 -print -quit)" ]] ||
  fail "published release contains writable entries"

# Neither a mutable alias nor an ordinary source-bound serve command is a
# valid way to enter the current product.
ln -s -- releases/0.2.0 "$INSTALL_ROOT/current"
if run_operation "$INSTALL_ROOT/current/native/status.sh" >"$TEST_ROOT/alias-status.out" 2>&1; then
  fail "an operational script accepted a mutable release alias"
fi
rm -- "$INSTALL_ROOT/current"
if run_operation "$INSTALL_ROOT/releases/0.2.0/bin/sentinel-monitor" serve \
  >"$TEST_ROOT/ordinary-serve.out" 2>&1; then
  fail "a source-bound Sentinel binary accepted ordinary serve"
fi

# A physical version is a one-shot destination even when all supplied bytes
# are identical. Rejection must not change the installed tree.
FIRST_MANIFEST="$(sha256sum "$INSTALL_ROOT/releases/0.2.0/RELEASE-MANIFEST" | awk '{print $1}')"
FIRST_LAYOUT="$(find -P "$INSTALL_ROOT" -printf '%P %y %m %s\n' | LC_ALL=C sort | sha256sum | awk '{print $1}')"
if run_build >"$TEST_ROOT/second-build.out" 2>&1; then
  fail "publisher accepted a second Sentinel 0.2.0 publication"
fi
grep -q 'one-shot' "$TEST_ROOT/second-build.out" ||
  fail "second publication did not identify the one-shot boundary"
[[ "$(sha256sum "$INSTALL_ROOT/releases/0.2.0/RELEASE-MANIFEST" | awk '{print $1}')" == "$FIRST_MANIFEST" ]] ||
  fail "rejected second publication changed the release manifest"
[[ "$(find -P "$INSTALL_ROOT" -printf '%P %y %m %s\n' | LC_ALL=C sort | sha256sum | awk '{print $1}')" == "$FIRST_LAYOUT" ]] ||
  fail "rejected second publication changed the installed layout"

# A symlinked deployment parent is rejected before publication.
EVIL_INSTALL="$TEST_ROOT/evil/opt/isarmg/sentinel-monitor"
mkdir -p -- "$EVIL_INSTALL" "$TEST_ROOT/evil-target"
ln -s -- "$TEST_ROOT/evil-target" "$EVIL_INSTALL/releases"
if env \
  PATH="$FAKE_BIN:$PATH" \
  SENTINEL_NATIVE_INSTALL_ROOT="$EVIL_INSTALL" \
  SENTINEL_NATIVE_TEST_ROOT="$TEST_ROOT" \
  SENTINEL_BUILD_TARGET="$TEST_ROOT/evil-build" \
  SENTINEL_MEDIAMTX_SOURCE="$FAKE_MEDIA" \
  SENTINEL_APP_BINARY_SOURCE="$FAKE_APP" \
  SENTINEL_WEB_SOURCE="$SOURCE_FIXTURE/web-dist" \
  FAKE_STATIC_CONTRACT="$FAKE_STATIC_CONTRACT" \
  "$SOURCE_FIXTURE/native/build.sh" >"$TEST_ROOT/symlink-build.out" 2>&1; then
  fail "build accepted a symlinked releases directory"
fi

# A configuration path with an intermediate symlink is rejected.
mkdir -p -- "$TEST_ROOT/bad-config-real"
ln -s -- "$TEST_ROOT/bad-config-real" "$TEST_ROOT/bad-config-link"
if env \
  PATH="$FAKE_BIN:$PATH" \
  SENTINEL_NATIVE_INSTALL_ROOT="$INSTALL_ROOT" \
  SENTINEL_NATIVE_CONFIG_DIR="$TEST_ROOT/bad-config-link" \
  SENTINEL_NATIVE_STATE_DIR="$STATE_ROOT" \
  SENTINEL_NATIVE_RUNTIME_DIR="$RUNTIME_ROOT" \
  "$INSTALL_ROOT/releases/0.2.0/native/bootstrap.sh" >"$TEST_ROOT/symlink-config.out" 2>&1; then
  fail "bootstrap accepted a symlinked configuration path"
fi

# A final configuration file symlink is never treated as an existing config.
BAD_FINAL_CONFIG="$TEST_ROOT/bad-final-config"
mkdir -p -- "$BAD_FINAL_CONFIG"
chmod 0755 -- "$BAD_FINAL_CONFIG"
printf '%s\n' 'must-not-be-read' >"$TEST_ROOT/symlink-env-target"
ln -s -- "$TEST_ROOT/symlink-env-target" "$BAD_FINAL_CONFIG/sentinel-monitor.env"
if env \
  PATH="$FAKE_BIN:$PATH" \
  SENTINEL_NATIVE_INSTALL_ROOT="$INSTALL_ROOT" \
  SENTINEL_NATIVE_CONFIG_DIR="$BAD_FINAL_CONFIG" \
  SENTINEL_NATIVE_STATE_DIR="$STATE_ROOT" \
  SENTINEL_NATIVE_RUNTIME_DIR="$RUNTIME_ROOT" \
  "$INSTALL_ROOT/releases/0.2.0/native/bootstrap.sh" >"$TEST_ROOT/symlink-env.out" 2>&1; then
  fail "bootstrap accepted a symbolic-link environment file"
fi

BOOTSTRAP_OUTPUT="$TEST_ROOT/bootstrap.out"
run_operation "$INSTALL_ROOT/releases/0.2.0/native/bootstrap.sh" >"$BOOTSTRAP_OUTPUT"
ENV_FILE="$CONFIG_ROOT/sentinel-monitor.env"
[[ "$(stat -c '%a' "$ENV_FILE")" == "600" ]] || fail "environment file is not mode 0600"
grep -q '^STATIC_DIR=.*/releases/0.2.0/web$' "$ENV_FILE" || fail "STATIC_DIR is not release-pinned"
JWT_VALUE="$(sed -n 's/^APP_JWT_SECRET=//p' "$ENV_FILE")"
KEY_VALUE="$(sed -n 's/^CREDENTIALS_KEY=//p' "$ENV_FILE")"
PASSWORD_VALUE="$(sed -n 's/^BOOTSTRAP_ADMIN_PASSWORD=//p' "$ENV_FILE")"
for secret in "$JWT_VALUE" "$KEY_VALUE" "$PASSWORD_VALUE"; do
  if grep -Fq -- "$secret" "$BOOTSTRAP_OUTPUT"; then
    fail "bootstrap printed a generated secret"
  fi
done

ENV_DIGEST="$(sha256sum "$ENV_FILE" | awk '{print $1}')"
run_operation "$INSTALL_ROOT/releases/0.2.0/native/bootstrap.sh" >/dev/null
[[ "$(sha256sum "$ENV_FILE" | awk '{print $1}')" == "$ENV_DIGEST" ]] ||
  fail "bootstrap overwrote an existing environment file"
if run_operation "$INSTALL_ROOT/releases/0.2.0/native/start.sh" >"$TEST_ROOT/unconfirmed.out" 2>&1; then
  fail "start accepted an unconfirmed generated administrator password"
fi

sed -i 's/^BOOTSTRAP_ADMIN_PASSWORD=.*/BOOTSTRAP_ADMIN_PASSWORD=operator-reviewed-password-0.2.0/' "$ENV_FILE"
chmod 0600 -- "$ENV_FILE"
run_operation "$INSTALL_ROOT/releases/0.2.0/native/bootstrap.sh" --confirm-config >/dev/null
[[ ! -e "$CONFIG_ROOT/sentinel-monitor.REVIEW-SECRETS-BEFORE-START" ]] || fail "review marker was not cleared"

# Failure after spawning the companion rolls back only this invocation and
# leaves no stale PID file or surviving process.
FAILED_MEDIA_PID_AUDIT="$TEST_ROOT/failed-media.pid"
if run_operation env \
  SENTINEL_TEST_CURL_FAILURE=1 \
  FAKE_MEDIA_PID_AUDIT="$FAILED_MEDIA_PID_AUDIT" \
  "$INSTALL_ROOT/releases/0.2.0/native/start.sh" >"$TEST_ROOT/readiness-failure.out" 2>&1; then
  fail "start succeeded while its readiness probe failed"
fi
[[ -s "$FAILED_MEDIA_PID_AUDIT" ]] || fail "failed start never launched the companion fixture"
FAILED_MEDIA_PID="$(<"$FAILED_MEDIA_PID_AUDIT")"
if kill -0 "$FAILED_MEDIA_PID" 2>/dev/null; then
  fail "failed start left its companion process running"
fi
[[ ! -e "$RUNTIME_ROOT/mediamtx.pid" ]] || fail "failed start left a companion PID file"
[[ ! -e "$RUNTIME_ROOT/app.pid" ]] || fail "failed start left an application PID file"

# start and stop share a short-lived operation lock, so concurrent launchers
# cannot race while publishing PID files.
(
  exec 8<>"$RUNTIME_ROOT/operations.lock"
  flock --exclusive 8
  # The child closes FD 8; only this subshell owns the test lock.
  sleep 30 8>&-
) &
OPERATION_LOCK_PID="$!"
OPERATION_LOCK_HELD=false
for _ in {1..20}; do
  if ! flock --exclusive --nonblock "$RUNTIME_ROOT/operations.lock" -c true; then
    OPERATION_LOCK_HELD=true
    break
  fi
  sleep 0.01
done
[[ "$OPERATION_LOCK_HELD" == true ]] || fail "operation-lock fixture did not acquire its lock"
START_IGNORED_OPERATION_LOCK=false
if run_operation "$INSTALL_ROOT/releases/0.2.0/native/start.sh" >"$TEST_ROOT/operation-lock.out" 2>&1; then
  START_IGNORED_OPERATION_LOCK=true
fi
kill "$OPERATION_LOCK_PID" 2>/dev/null || true
wait "$OPERATION_LOCK_PID" 2>/dev/null || true
OPERATION_LOCK_PID=""
[[ "$START_IGNORED_OPERATION_LOCK" != true ]] || fail "start ignored another active native operation"

# Runtime entries and artifacts must remain complete after the source fixture disappears.
chmod -R u+w -- "$SOURCE_FIXTURE"
rm -rf -- "$SOURCE_FIXTURE"
run_operation "$INSTALL_ROOT/releases/0.2.0/native/start.sh" >/dev/null
for _ in {1..40}; do
  [[ -s "$RUNTIME_ROOT/app.pid" && -s "$RUNTIME_ROOT/mediamtx.pid" ]] && break
  sleep 0.05
done
[[ -s "$RUNTIME_ROOT/app.pid" && -s "$RUNTIME_ROOT/mediamtx.pid" ]] ||
  fail "release processes did not publish their PID files"
STATUS_OUTPUT="$(run_operation "$INSTALL_ROOT/releases/0.2.0/native/status.sh")"
[[ "$STATUS_OUTPUT" == *'Rust application: running'* ]] || fail "status missed the application"
[[ "$STATUS_OUTPUT" == *'MediaMTX: running'* ]] || fail "status missed MediaMTX"
run_operation "$INSTALL_ROOT/releases/0.2.0/native/stop.sh" >/dev/null
STATUS_OUTPUT="$(run_operation "$INSTALL_ROOT/releases/0.2.0/native/status.sh")"
[[ "$STATUS_OUTPUT" == *'Rust application: stopped'* ]] || fail "application did not stop"
[[ "$STATUS_OUTPUT" == *'MediaMTX: stopped'* ]] || fail "MediaMTX did not stop"

# Existing release files with hard-link aliases fail closed.
RELEASE_ROOT="$INSTALL_ROOT/releases/0.2.0"
chmod 0755 -- "$RELEASE_ROOT" "$RELEASE_ROOT/web" "$RELEASE_ROOT/web/assets"
ln -- "$RELEASE_ROOT/web/assets/app.js" "$TEST_ROOT/release-hardlink-alias"
chmod 0555 -- "$RELEASE_ROOT/web/assets" "$RELEASE_ROOT/web" "$RELEASE_ROOT"
if run_operation "$INSTALL_ROOT/releases/0.2.0/native/status.sh" >"$TEST_ROOT/hardlink.out" 2>&1; then
  fail "release verification accepted a hard-linked asset"
fi

grep -q '^sha256=25947caac403f37ec881c9be213af2cad67e344a6c7098905b0d31c17f40e336$' \
  "$REPOSITORY_ROOT/config/mediamtx.lock" || fail "the reviewed production MediaMTX digest changed"

echo "Sentinel native temporary-root lifecycle tests passed"
