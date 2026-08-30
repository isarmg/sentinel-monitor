#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="$ROOT_DIR/.env.native"
RUNTIME_DIR="${SENTINEL_RUNTIME_DIR:-/mnt/c/Users/micro/sentinel-runtime}"
DATABASE_PATH="$RUNTIME_DIR/data/sentinel.sqlite3"

mkdir -p "$RUNTIME_DIR/data" "$RUNTIME_DIR/logs" "$RUNTIME_DIR/recordings"

if [[ -f "$ENV_FILE" ]]; then
  set -a
  source "$ENV_FILE"
  set +a
  if [[ "${DATABASE_URL:-}" != sqlite://* ]]; then
    echo "Existing .env.native must use a sqlite:// DATABASE_URL" >&2
    exit 1
  fi
  ADMIN_PASSWORD="${BOOTSTRAP_ADMIN_PASSWORD:-unchanged}"
else
  umask 077
  JWT_SECRET="$(openssl rand -hex 48)"
  CREDENTIAL_KEY="$(openssl rand -base64 32 | tr -d '\n')"
  ADMIN_PASSWORD="$(openssl rand -base64 24 | tr -d '/+=\n' | cut -c1-24)"

  cat >"$ENV_FILE" <<EOF
BIND_ADDR=0.0.0.0:8080
DATABASE_URL=sqlite://${DATABASE_PATH}
APP_JWT_SECRET=${JWT_SECRET}
CREDENTIALS_KEY=${CREDENTIAL_KEY}
CREDENTIALS_KEY_ID=local://sentinel/credentials-key/v1
BOOTSTRAP_ADMIN_EMAIL=admin@sentinel.local
BOOTSTRAP_ADMIN_PASSWORD=${ADMIN_PASSWORD}
APP_ENV=production
SESSION_IDLE_TTL_MINUTES=30
SESSION_ABSOLUTE_TTL_HOURS=12
LOGIN_BODY_LIMIT_BYTES=16384
LOGIN_RATE_CAPACITY=4096
LOGIN_SOURCE_ATTEMPTS=30
LOGIN_SOURCE_WINDOW_SECS=60
LOGIN_ACCOUNT_ATTEMPTS=10
LOGIN_ACCOUNT_WINDOW_SECS=300
LOGIN_ARGON2_PARALLELISM=2
LOGIN_ARGON2_TIMEOUT_MS=5000
MEDIAMTX_API_URL=http://127.0.0.1:9997
MEDIAMTX_PLAYBACK_URL=http://127.0.0.1:9996
MEDIAMTX_CONFIG=${ROOT_DIR}/native/mediamtx.yml
MEDIAMTX_CONTRACT=${ROOT_DIR}/native/mediamtx.lock
MEDIAMTX_BINARY=${RUNTIME_DIR}/bin/mediamtx
RECORDINGS_DIR=${RUNTIME_DIR}/recordings
SENTINEL_RUNTIME_DIR=${RUNTIME_DIR}
PUBLIC_WEBRTC_BASE_URL=http://127.0.0.1:8889
PUBLIC_HLS_BASE_URL=http://127.0.0.1:8888
STATIC_DIR=/mnt/sarmg.org/sentinel-monitor/web/dist
STATUS_INTERVAL_SECS=10
RECONCILE_INTERVAL_SECS=60
RUST_LOG=info,tower_http=info
EOF
  chmod 600 "$ENV_FILE"
fi

echo "Database: ${DATABASE_URL:-sqlite://${DATABASE_PATH}}"
echo "Administrator: ${BOOTSTRAP_ADMIN_EMAIL:-admin@sentinel.local}"
echo "Temporary administrator password: $ADMIN_PASSWORD"
