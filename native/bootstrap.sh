#!/usr/bin/env bash
set -euo pipefail

SCRIPT_PATH="${BASH_SOURCE[0]}"
# shellcheck source=/dev/null
source "$(cd "$(dirname "$SCRIPT_PATH")" && pwd -P)/common.sh"
resolve_release_context "$SCRIPT_PATH"
deployment_paths
verify_release "$SENTINEL_RELEASE_ROOT"

if (( $# == 0 )); then
  MODE="create"
elif (( $# == 1 )) && [[ "$1" == "--confirm-config" ]]; then
  MODE="confirm"
else
  die "Usage: bootstrap.sh [--confirm-config]"
fi

ensure_directory "$(dirname "$SENTINEL_CONFIG_DIR")" 755 "configuration parent"
ensure_directory "$SENTINEL_CONFIG_DIR" 755 "Sarmg configuration directory"
ensure_directory "$(dirname "$SENTINEL_STATE_DIR")" 755 "state parent"
ensure_directory "$SENTINEL_STATE_DIR" 700 "state directory"
ensure_directory "$SENTINEL_STATE_DIR/db" 700 "database directory"
ensure_directory "$SENTINEL_STATE_DIR/recordings" 700 "recordings directory"
ensure_directory "$SENTINEL_STATE_DIR/logs" 700 "log directory"
ensure_directory "$(dirname "$SENTINEL_RUNTIME_PATH")" 755 "runtime parent"
ensure_directory "$SENTINEL_RUNTIME_PATH" 700 "runtime directory"

if [[ "$MODE" == "confirm" ]]; then
  load_deployment_env
  require_runtime_contract
  if [[ -e "$SENTINEL_REVIEW_MARKER" || -L "$SENTINEL_REVIEW_MARKER" ]]; then
    assert_private_file "$SENTINEL_REVIEW_MARKER" "configuration review marker"
    rm -- "$SENTINEL_REVIEW_MARKER"
  fi
  echo "Sentinel 0.2.0 configuration accepted. Start it with: $SENTINEL_RELEASE_ROOT/native/start.sh"
  exit 0
fi

if [[ -e "$SENTINEL_ENV_FILE" || -L "$SENTINEL_ENV_FILE" ]]; then
  assert_private_file "$SENTINEL_ENV_FILE" "Sentinel environment file"
  echo "Configuration already exists and was not changed: $SENTINEL_ENV_FILE"
  if [[ -e "$SENTINEL_REVIEW_MARKER" || -L "$SENTINEL_REVIEW_MARKER" ]]; then
    assert_private_file "$SENTINEL_REVIEW_MARKER" "configuration review marker"
    echo "Review it, then run: $SENTINEL_RELEASE_ROOT/native/bootstrap.sh --confirm-config"
  fi
  exit 0
fi

require_command openssl
JWT_SECRET="$(openssl rand -hex 48)"
CREDENTIAL_KEY="$(openssl rand -base64 32 | tr -d '\n')"
ADMIN_PASSWORD="$(openssl rand -hex 24)"

# Publish the review gate before the configuration. A crash can therefore
# leave the deployment unconfigured or unconfirmed, but never startable with
# an administrator password the operator has not reviewed.
if [[ -e "$SENTINEL_REVIEW_MARKER" || -L "$SENTINEL_REVIEW_MARKER" ]]; then
  assert_private_file "$SENTINEL_REVIEW_MARKER" "configuration review marker"
else
  (umask 077; set -o noclobber; : >"$SENTINEL_REVIEW_MARKER") ||
    die "Failed to create the configuration review marker"
  chmod 600 -- "$SENTINEL_REVIEW_MARKER"
  assert_private_file "$SENTINEL_REVIEW_MARKER" "configuration review marker"
fi

CONFIG_TEMP="$(mktemp "$SENTINEL_CONFIG_DIR/.sentinel-monitor.env.XXXXXX")"
cleanup_config_temp() {
  if [[ -n "${CONFIG_TEMP:-}" && ( -e "$CONFIG_TEMP" || -L "$CONFIG_TEMP" ) ]]; then
    rm -- "$CONFIG_TEMP"
  fi
}
trap cleanup_config_temp EXIT
(
  umask 077
  {
    echo "BIND_ADDR=127.0.0.1:8080"
    echo "DATABASE_URL=sqlite://$SENTINEL_STATE_DIR/db/app.db"
    echo "APP_JWT_SECRET=$JWT_SECRET"
    echo "CREDENTIALS_KEY=$CREDENTIAL_KEY"
    echo "BOOTSTRAP_ADMIN_USERNAME=admin"
    echo "BOOTSTRAP_ADMIN_PASSWORD=$ADMIN_PASSWORD"
    echo "APP_ENV=production"
    echo "SESSION_IDLE_TTL_MINUTES=30"
    echo "SESSION_ABSOLUTE_TTL_HOURS=12"
    echo "LOGIN_BODY_LIMIT_BYTES=16384"
    echo "LOGIN_RATE_CAPACITY=4096"
    echo "LOGIN_SOURCE_ATTEMPTS=30"
    echo "LOGIN_SOURCE_WINDOW_SECS=60"
    echo "LOGIN_ACCOUNT_ATTEMPTS=10"
    echo "LOGIN_ACCOUNT_WINDOW_SECS=300"
    echo "LOGIN_ARGON2_PARALLELISM=2"
    echo "LOGIN_ARGON2_TIMEOUT_MS=5000"
    echo "MEDIAMTX_API_URL=http://127.0.0.1:9997"
    echo "MEDIAMTX_PLAYBACK_URL=http://127.0.0.1:9996"
    echo "MEDIAMTX_CONFIG=$SENTINEL_RELEASE_ROOT/config/mediamtx.yml"
    echo "MEDIAMTX_CONTRACT=$SENTINEL_RELEASE_ROOT/config/mediamtx.lock"
    echo "MEDIAMTX_BINARY=$SENTINEL_RELEASE_ROOT/bin/mediamtx"
    echo "RECORDINGS_DIR=$SENTINEL_STATE_DIR/recordings"
    echo "SENTINEL_RUNTIME_DIR=$SENTINEL_RUNTIME_PATH"
    echo "PUBLIC_WEBRTC_BASE_URL=/media-webrtc"
    echo "PUBLIC_HLS_BASE_URL=/media-hls"
    echo "MEDIA_PUBLIC_HOSTS=127.0.0.1"
    echo "STATIC_DIR=$SENTINEL_RELEASE_ROOT/web"
    echo "STATUS_INTERVAL_SECS=10"
    echo "RECONCILE_INTERVAL_SECS=60"
    echo "REQUEST_TIMEOUT_SECS=20"
    echo "RUST_LOG=info,tower_http=info"
  } >"$CONFIG_TEMP"
)
chmod 600 -- "$CONFIG_TEMP"
assert_private_file "$CONFIG_TEMP" "temporary Sentinel environment file"

# link(2) supplies no-overwrite publication in the same private directory.
# Until the temporary name is removed the nlink check keeps consumers closed.
ln -- "$CONFIG_TEMP" "$SENTINEL_ENV_FILE" ||
  die "Sentinel environment file appeared during bootstrap; it was not overwritten"
rm -- "$CONFIG_TEMP"
CONFIG_TEMP=""
trap - EXIT
assert_private_file "$SENTINEL_ENV_FILE" "Sentinel environment file"
unset JWT_SECRET CREDENTIAL_KEY ADMIN_PASSWORD

echo "Created private configuration without printing its secrets: $SENTINEL_ENV_FILE"
echo "Replace the generated administrator password and review every setting with a protected editor."
echo "Then run: $SENTINEL_RELEASE_ROOT/native/bootstrap.sh --confirm-config"
