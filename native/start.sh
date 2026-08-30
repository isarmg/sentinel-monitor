#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNTIME_DIR="${SENTINEL_RUNTIME_DIR:-/mnt/c/Users/micro/sentinel-runtime}"
ENV_FILE="$ROOT_DIR/.env.native"
MEDIA_BIN="$RUNTIME_DIR/bin/mediamtx"
APP_BIN="$RUNTIME_DIR/bin/sentinel-monitor"
MEDIA_LOCK="$ROOT_DIR/native/mediamtx.lock"

if [[ ! -f "$ENV_FILE" ]]; then
  echo "Missing $ENV_FILE" >&2
  exit 1
fi
if [[ ! -x "$MEDIA_BIN" ]]; then
  echo "Missing MediaMTX binary: $MEDIA_BIN" >&2
  exit 1
fi
if [[ ! -x "$APP_BIN" ]]; then
  echo "Missing Rust application: $APP_BIN" >&2
  exit 1
fi
if [[ ! -r "$MEDIA_LOCK" ]]; then
  echo "Missing MediaMTX companion lock: $MEDIA_LOCK" >&2
  exit 1
fi
if ! command -v flock >/dev/null 2>&1; then
  echo "Missing required flock utility" >&2
  exit 1
fi

locked_value() {
  awk -F= -v key="$1" '$1 == key { print $2 }' "$MEDIA_LOCK"
}

EXPECTED_MEDIA_VERSION="$(locked_value version)"
EXPECTED_MEDIA_PLATFORM="$(locked_value platform)"
EXPECTED_MEDIA_SHA256="$(locked_value sha256)"
ACTUAL_MEDIA_VERSION="$("$MEDIA_BIN" --version | tr -d '\r\n')"
ACTUAL_MEDIA_SHA256="$(sha256sum "$MEDIA_BIN" | awk '{print $1}')"
if [[ "$EXPECTED_MEDIA_PLATFORM" != "linux_amd64" ]]; then
  echo "Unsupported MediaMTX companion platform: $EXPECTED_MEDIA_PLATFORM" >&2
  exit 1
fi
if [[ "$ACTUAL_MEDIA_VERSION" != "$EXPECTED_MEDIA_VERSION" ]]; then
  echo "MediaMTX version mismatch: expected $EXPECTED_MEDIA_VERSION, got $ACTUAL_MEDIA_VERSION" >&2
  exit 1
fi
if [[ "$ACTUAL_MEDIA_SHA256" != "$EXPECTED_MEDIA_SHA256" ]]; then
  echo "MediaMTX SHA-256 mismatch; refusing to start an unapproved companion" >&2
  exit 1
fi

set -a
source "$ENV_FILE"
set +a
export STATIC_DIR="$ROOT_DIR/web/dist"

umask 077
mkdir -p "$RUNTIME_DIR/data" "$RUNTIME_DIR/logs" "$RUNTIME_DIR/recordings"
touch "$RUNTIME_DIR/app.lock" "$RUNTIME_DIR/mediamtx.lock"
chmod 600 "$RUNTIME_DIR/app.lock" "$RUNTIME_DIR/mediamtx.lock"
WSL_ADDRESS="$(hostname -I | awk '{print $1}')"
MEDIA_HOSTS="${MEDIA_PUBLIC_HOSTS:-$WSL_ADDRESS}"

is_running() {
  local pid_file="$1"
  [[ -f "$pid_file" ]] && kill -0 "$(cat "$pid_file")" 2>/dev/null
}

if ! is_running "$RUNTIME_DIR/mediamtx.pid"; then
  # --no-fork execs MediaMTX in this PID while retaining flock's descriptor,
  # so the lock is held for the companion's complete lifetime.
  nohup flock --no-fork --nonblock "$RUNTIME_DIR/mediamtx.lock" \
    env MTX_WEBRTCADDITIONALHOSTS="$MEDIA_HOSTS" \
    "$MEDIA_BIN" "$ROOT_DIR/native/mediamtx.yml" \
    >"$RUNTIME_DIR/logs/mediamtx.log" 2>&1 &
  echo $! >"$RUNTIME_DIR/mediamtx.pid"
fi

for _ in {1..30}; do
  if curl -fsS http://127.0.0.1:9997/v3/info >/dev/null; then
    break
  fi
  sleep 0.25
done

if ! is_running "$RUNTIME_DIR/app.pid"; then
  cd "$ROOT_DIR"
  # The inherited lock descriptor remains open after flock execs the Rust app.
  nohup flock --no-fork --nonblock "$RUNTIME_DIR/app.lock" \
    "$APP_BIN" >"$RUNTIME_DIR/logs/app.log" 2>&1 &
  echo $! >"$RUNTIME_DIR/app.pid"
fi

for _ in {1..60}; do
  if curl -fsS http://127.0.0.1:8080/health/ready >/dev/null; then
    echo "Sentinel Monitor is ready at http://127.0.0.1:8080"
    echo "WSL media candidate address: $MEDIA_HOSTS"
    exit 0
  fi
  sleep 0.25
done

echo "Startup did not become ready; inspect $RUNTIME_DIR/logs/app.log" >&2
exit 1
