#!/usr/bin/env bash
set -euo pipefail

REPOSITORY_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
# shellcheck source=/dev/null
source "$REPOSITORY_ROOT/native/common.sh"

TEST_ROOT="$(mktemp -d)"
APP_PID=""
cleanup() {
  if [[ -n "$APP_PID" ]]; then
    kill "$APP_PID" 2>/dev/null || true
    wait "$APP_PID" 2>/dev/null || true
  fi
  chmod -R u+w -- "$TEST_ROOT" 2>/dev/null || true
  rm -rf -- "$TEST_ROOT"
}
trap cleanup EXIT

WEB_ROOT="$TEST_ROOT/release/web"
APP_ROOT="$TEST_ROOT/release/bin"
STATE_ROOT="$TEST_ROOT/state"
RUNTIME_ROOT="$TEST_ROOT/runtime"
mkdir -p -- "$WEB_ROOT" "$APP_ROOT" "$STATE_ROOT/db" "$RUNTIME_ROOT"

npm ci --prefix "$REPOSITORY_ROOT/web" >/dev/null
npm run build --prefix "$REPOSITORY_ROOT/web" -- \
  --outDir "$WEB_ROOT" --emptyOutDir >/dev/null
find -P "$WEB_ROOT" -type d -exec chmod 0555 -- {} +
find -P "$WEB_ROOT" -type f -exec chmod 0444 -- {} +

STATIC_MANIFEST="$TEST_ROOT/static-layout.manifest"
write_static_manifest "$WEB_ROOT" "$STATIC_MANIFEST"
EXPECTED_CONTRACT="$(sha256sum -- "$STATIC_MANIFEST" | awk '{print $1}')"
SENTINEL_STATIC_MANIFEST_PATH="$STATIC_MANIFEST" \
  CARGO_PROFILE_DEV_DEBUG=0 \
  cargo build --locked --manifest-path "$REPOSITORY_ROOT/Cargo.toml" >/dev/null
install -m 0555 -- "$REPOSITORY_ROOT/target/debug/sentinel-monitor" \
  "$APP_ROOT/sentinel-monitor"
ACTUAL_CONTRACT="$("$APP_ROOT/sentinel-monitor" static-contract | tr -d '\r\n')"
[[ "$ACTUAL_CONTRACT" == "$EXPECTED_CONTRACT" ]] || {
  echo "Relocated binary has the wrong static contract" >&2
  exit 1
}

PORT="$((24000 + (BASHPID % 10000)))"
APP_LOG="$TEST_ROOT/app.log"
(
  cd /
  env \
    BIND_ADDR="127.0.0.1:$PORT" \
    DATABASE_URL="sqlite://$STATE_ROOT/db/app.db" \
    APP_JWT_SECRET="relocated-smoke-jwt-secret-at-least-32-bytes" \
    CREDENTIALS_KEY="AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=" \
    BOOTSTRAP_ADMIN_EMAIL="smoke@example.invalid" \
    BOOTSTRAP_ADMIN_PASSWORD="relocated-smoke-bootstrap-password" \
    APP_ENV=production \
    SENTINEL_RUNTIME_DIR="$RUNTIME_ROOT" \
    STATIC_DIR="$WEB_ROOT" \
    MEDIAMTX_API_URL="http://127.0.0.1:9" \
    MEDIAMTX_PLAYBACK_URL="http://127.0.0.1:9" \
    STATUS_INTERVAL_SECS=60 \
    RECONCILE_INTERVAL_SECS=60 \
    REQUEST_TIMEOUT_SECS=1 \
    RUST_LOG=warn \
    "$APP_ROOT/sentinel-monitor" serve >"$APP_LOG" 2>&1
) &
APP_PID="$!"

READY=false
for _ in {1..120}; do
  if curl --fail --silent "http://127.0.0.1:$PORT/health/live" >/dev/null; then
    READY=true
    break
  fi
  if ! kill -0 "$APP_PID" 2>/dev/null; then
    break
  fi
  sleep 0.1
done
if [[ "$READY" != true ]]; then
  sed -n '1,160p' "$APP_LOG" >&2
  echo "Relocated Sentinel binary did not start" >&2
  exit 1
fi

INDEX="$TEST_ROOT/index.html"
curl --fail --silent --show-error "http://127.0.0.1:$PORT/" >"$INDEX"
mapfile -t ASSETS < <(grep -oE '/assets/[A-Za-z0-9._-]+' "$INDEX" | LC_ALL=C sort -u)
(( ${#ASSETS[@]} >= 2 )) || {
  echo "Relocated index did not reference the expected hashed assets" >&2
  exit 1
}
for asset in "${ASSETS[@]}"; do
  curl --fail --silent --show-error "http://127.0.0.1:$PORT$asset" >/dev/null
done

kill "$APP_PID"
wait "$APP_PID" 2>/dev/null || true
APP_PID=""

ASSET_TO_TAMPER="$WEB_ROOT${ASSETS[0]}"
chmod 0644 -- "$ASSET_TO_TAMPER"
printf '%s\n' 'tampered' >"$ASSET_TO_TAMPER"
if env \
  BIND_ADDR="127.0.0.1:$PORT" \
  DATABASE_URL="sqlite://$STATE_ROOT/db/app.db" \
  APP_JWT_SECRET="relocated-smoke-jwt-secret-at-least-32-bytes" \
  CREDENTIALS_KEY="AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=" \
  APP_ENV=production \
  SENTINEL_RUNTIME_DIR="$RUNTIME_ROOT" \
  STATIC_DIR="$WEB_ROOT" \
  "$APP_ROOT/sentinel-monitor" serve >"$TEST_ROOT/tamper.log" 2>&1; then
  echo "Tampered static assets were accepted" >&2
  exit 1
fi
grep -q 'static asset contract' "$TEST_ROOT/tamper.log" || {
  echo "Tampered static asset rejection did not identify the contract boundary" >&2
  exit 1
}

echo "Sentinel relocated binary and exact static asset smoke tests passed"
