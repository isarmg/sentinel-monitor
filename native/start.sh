#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNTIME_DIR="${SENTINEL_RUNTIME_DIR:-/mnt/c/Users/micro/sentinel-runtime}"
ENV_FILE="$ROOT_DIR/.env.native"
MEDIA_BIN="$RUNTIME_DIR/bin/mediamtx"
APP_BIN="$RUNTIME_DIR/bin/sentinel-monitor"

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
if ! pg_isready -q; then
  echo "PostgreSQL is not ready" >&2
  exit 1
fi

set -a
source "$ENV_FILE"
set +a
export STATIC_DIR="$ROOT_DIR/web/dist"

mkdir -p "$RUNTIME_DIR/logs" "$RUNTIME_DIR/recordings"
WSL_ADDRESS="$(hostname -I | awk '{print $1}')"
MEDIA_HOSTS="${MEDIA_PUBLIC_HOSTS:-$WSL_ADDRESS}"

is_running() {
  local pid_file="$1"
  [[ -f "$pid_file" ]] && kill -0 "$(cat "$pid_file")" 2>/dev/null
}

if ! is_running "$RUNTIME_DIR/mediamtx.pid"; then
  nohup env MTX_WEBRTCADDITIONALHOSTS="$MEDIA_HOSTS" \
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
  nohup "$APP_BIN" >"$RUNTIME_DIR/logs/app.log" 2>&1 &
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
