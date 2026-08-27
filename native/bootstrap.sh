#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="$ROOT_DIR/.env.native"

if [[ -f "$ENV_FILE" ]]; then
  set -a
  source "$ENV_FILE"
  set +a
  DB_PASSWORD="${DATABASE_URL#postgres://monitor:}"
  DB_PASSWORD="${DB_PASSWORD%@*}"
  ADMIN_PASSWORD="$BOOTSTRAP_ADMIN_PASSWORD"
else
  umask 077
  DB_PASSWORD="$(openssl rand -hex 24)"
  JWT_SECRET="$(openssl rand -hex 48)"
  CREDENTIAL_KEY="$(openssl rand -base64 32 | tr -d '\n')"
  ADMIN_PASSWORD="$(openssl rand -base64 24 | tr -d '/+=\n' | cut -c1-24)"

  cat >"$ENV_FILE" <<EOF
BIND_ADDR=0.0.0.0:8080
DATABASE_URL=postgres://monitor:${DB_PASSWORD}@127.0.0.1:5432/monitor
APP_JWT_SECRET=${JWT_SECRET}
CREDENTIALS_KEY=${CREDENTIAL_KEY}
BOOTSTRAP_ADMIN_EMAIL=admin@sentinel.local
BOOTSTRAP_ADMIN_PASSWORD=${ADMIN_PASSWORD}
SESSION_COOKIE_SECURE=false
MEDIAMTX_API_URL=http://127.0.0.1:9997
MEDIAMTX_PLAYBACK_URL=http://127.0.0.1:9996
PUBLIC_WEBRTC_BASE_URL=http://127.0.0.1:8889
PUBLIC_HLS_BASE_URL=http://127.0.0.1:8888
STATIC_DIR=/mnt/sarmg.org/sentinel-monitor/web/dist
STATUS_INTERVAL_SECS=10
RECONCILE_INTERVAL_SECS=60
RUST_LOG=info,tower_http=info
EOF
  chmod 600 "$ENV_FILE"
fi

/usr/sbin/runuser -u postgres -- /usr/bin/psql \
  --set=ON_ERROR_STOP=1 \
  --set=db_password="$DB_PASSWORD" \
  --dbname=postgres <<'SQL'
SELECT format('CREATE ROLE monitor LOGIN PASSWORD %L', :'db_password')
WHERE NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'monitor') \gexec
SELECT format('ALTER ROLE monitor WITH LOGIN PASSWORD %L', :'db_password') \gexec
SELECT 'CREATE DATABASE monitor OWNER monitor'
WHERE NOT EXISTS (SELECT FROM pg_database WHERE datname = 'monitor') \gexec
SELECT 'ALTER DATABASE monitor OWNER TO monitor' \gexec
SQL

echo "Database: monitor"
echo "Administrator: admin@sentinel.local"
echo "Temporary administrator password: $ADMIN_PASSWORD"
