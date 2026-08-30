#!/usr/bin/env bash
set -euo pipefail

SCRIPT_PATH="${BASH_SOURCE[0]}"
# shellcheck source=/dev/null
source "$(cd "$(dirname "$SCRIPT_PATH")" && pwd -P)/common.sh"
resolve_release_context "$SCRIPT_PATH"
deployment_paths
verify_release "$SENTINEL_RELEASE_ROOT"

[[ ! -e "$SENTINEL_REVIEW_MARKER" && ! -L "$SENTINEL_REVIEW_MARKER" ]] ||
  die "Configuration has not been confirmed; run bootstrap.sh --confirm-config"
load_deployment_env
require_runtime_contract
assert_private_directory "$SENTINEL_STATE_DIR" "state directory"
assert_private_directory "$SENTINEL_STATE_DIR/db" "database directory"
assert_private_directory "$RECORDINGS_DIR" "recordings directory"
assert_private_directory "$SENTINEL_STATE_DIR/logs" "log directory"
assert_private_directory "$SENTINEL_RUNTIME_PATH" "runtime directory"
assert_regular_file "$MEDIAMTX_BINARY" "MediaMTX binary"
assert_regular_file "$MEDIAMTX_CONFIG" "MediaMTX configuration"
assert_regular_file "$MEDIAMTX_CONTRACT" "MediaMTX contract"
assert_regular_file "$SENTINEL_RELEASE_ROOT/bin/sentinel-monitor" "Sentinel binary"
require_command curl
require_command flock
umask 077
acquire_native_operation_lock

locked_value() {
  awk -F= -v key="$1" '$1 == key { print $2 }' "$MEDIAMTX_CONTRACT"
}

EXPECTED_MEDIA_VERSION="$(locked_value version)"
EXPECTED_MEDIA_PLATFORM="$(locked_value platform)"
EXPECTED_MEDIA_SHA256="$(locked_value sha256)"
ACTUAL_MEDIA_VERSION="$($MEDIAMTX_BINARY --version | tr -d '\r\n')"
ACTUAL_MEDIA_SHA256="$(sha256sum -- "$MEDIAMTX_BINARY" | awk '{print $1}')"
[[ "$EXPECTED_MEDIA_PLATFORM" == "linux_amd64" ]] ||
  die "Unsupported MediaMTX companion platform: $EXPECTED_MEDIA_PLATFORM"
[[ "$ACTUAL_MEDIA_VERSION" == "$EXPECTED_MEDIA_VERSION" ]] ||
  die "MediaMTX version mismatch"
[[ "$ACTUAL_MEDIA_SHA256" == "$EXPECTED_MEDIA_SHA256" ]] ||
  die "MediaMTX SHA-256 mismatch; refusing to start an unapproved companion"

APP_BIN="$SENTINEL_RELEASE_ROOT/bin/sentinel-monitor"
MEDIA_LOCK="$SENTINEL_RUNTIME_PATH/mediamtx.lock"
MEDIA_PID_FILE="$SENTINEL_RUNTIME_PATH/mediamtx.pid"
APP_PID_FILE="$SENTINEL_RUNTIME_PATH/app.pid"
ensure_lock_file "$SENTINEL_RUNTIME_PATH/app.lock" "application runtime lock"
ensure_lock_file "$MEDIA_LOCK" "MediaMTX runtime lock"

APP_PID="$(read_running_pid "$APP_PID_FILE" "$APP_BIN" || true)"
MEDIA_PID="$(read_running_pid "$MEDIA_PID_FILE" "$MEDIAMTX_BINARY" || true)"
STARTED_APP=""
STARTED_MEDIA=""
START_COMPLETE=false
remove_matching_pid_file() {
  local path="$1"
  local pid="$2"
  if [[ -f "$path" && ! -L "$path" ]] && [[ "$(<"$path")" == "$pid" ]]; then
    rm -- "$path"
  fi
}

rollback_start() {
  if [[ -n "$STARTED_APP" ]]; then
    terminate_started_process "$STARTED_APP"
    remove_matching_pid_file "$APP_PID_FILE" "$STARTED_APP"
  fi
  if [[ -n "$STARTED_MEDIA" ]]; then
    terminate_started_process "$STARTED_MEDIA"
    remove_matching_pid_file "$MEDIA_PID_FILE" "$STARTED_MEDIA"
  fi
}

terminate_started_process() {
  local pid="$1"
  kill "$pid" 2>/dev/null || true
  for _ in {1..40}; do
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.05
  done
  if kill -0 "$pid" 2>/dev/null; then
    kill -KILL "$pid" 2>/dev/null || true
  fi
  wait "$pid" 2>/dev/null || true
}

cleanup_failed_start() {
  local status="$?"
  trap - EXIT
  if [[ "$START_COMPLETE" != true ]]; then
    rollback_start
  fi
  exit "$status"
}
trap cleanup_failed_start EXIT

wait_for_pid_identity() {
  local path="$1"
  local binary="$2"
  local expected_pid="$3"
  local actual_pid
  for _ in {1..20}; do
    kill -0 "$expected_pid" 2>/dev/null || return 1
    actual_pid="$(read_running_pid "$path" "$binary" || true)"
    [[ "$actual_pid" == "$expected_pid" ]] && return 0
    sleep 0.05
  done
  return 1
}

# Neither long-lived process inherits the operator's checkout or working
# directory. Every runtime input below is an absolute release/state path.
cd /

if [[ -z "$MEDIA_PID" ]]; then
  if [[ -e "$MEDIA_PID_FILE" || -L "$MEDIA_PID_FILE" ]]; then
    assert_private_file "$MEDIA_PID_FILE" "MediaMTX PID file"
    rm -- "$MEDIA_PID_FILE"
  fi
  MEDIA_HOSTS="${MEDIA_PUBLIC_HOSTS:-127.0.0.1}"
  nohup flock --no-fork --nonblock "$MEDIA_LOCK" \
    env MTX_WEBRTCADDITIONALHOSTS="$MEDIA_HOSTS" \
    MTX_PATHDEFAULTS_RECORDPATH="$RECORDINGS_DIR/%path/%Y-%m-%d_%H-%M-%S-%f" \
    MTX_PATHDEFAULTS_RECORDDELETEAFTER="${RECORD_DELETE_AFTER:-168h}" \
    "$MEDIAMTX_BINARY" "$MEDIAMTX_CONFIG" \
    9>&- >"$SENTINEL_STATE_DIR/logs/mediamtx.log" 2>&1 &
  STARTED_MEDIA="$!"
  write_pid_file "$MEDIA_PID_FILE" "$STARTED_MEDIA"
fi

MEDIA_READY=false
for _ in {1..30}; do
  if curl -fsS "${MEDIAMTX_READY_URL:-http://127.0.0.1:9997/v3/info}" >/dev/null; then
    MEDIA_READY=true
    break
  fi
  sleep 0.25
done
[[ "$MEDIA_READY" == true ]] || die "MediaMTX did not become ready"
if [[ -n "$STARTED_MEDIA" ]]; then
  wait_for_pid_identity "$MEDIA_PID_FILE" "$MEDIAMTX_BINARY" "$STARTED_MEDIA" ||
    die "MediaMTX did not retain its expected PID identity"
fi

if [[ -z "$APP_PID" ]]; then
  if [[ -e "$APP_PID_FILE" || -L "$APP_PID_FILE" ]]; then
    assert_private_file "$APP_PID_FILE" "application PID file"
    rm -- "$APP_PID_FILE"
  fi
  nohup "$APP_BIN" serve-release "$SENTINEL_RELEASE_ROOT" \
    9>&- >"$SENTINEL_STATE_DIR/logs/app.log" 2>&1 &
  STARTED_APP="$!"
fi

APP_READY=false
for _ in {1..60}; do
  if curl -fsS "${SENTINEL_READY_URL:-http://127.0.0.1:8080/health/ready}" >/dev/null; then
    APP_READY=true
    break
  fi
  sleep 0.25
done
[[ "$APP_READY" == true ]] || die "Sentinel application did not become ready"
if [[ -n "$STARTED_APP" ]]; then
  wait_for_pid_identity "$APP_PID_FILE" "$APP_BIN" "$STARTED_APP" ||
    die "Sentinel application did not retain its expected PID identity"
fi
START_COMPLETE=true
trap - EXIT
STARTED_APP=""
STARTED_MEDIA=""

echo "Sentinel Monitor 0.2.0 is ready at ${SENTINEL_READY_URL:-http://127.0.0.1:8080/health/ready}"
