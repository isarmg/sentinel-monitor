#!/usr/bin/env bash
set -euo pipefail

SCRIPT_PATH="${BASH_SOURCE[0]}"
# shellcheck source=/dev/null
source "$(cd "$(dirname "$SCRIPT_PATH")" && pwd -P)/common.sh"
resolve_release_context "$SCRIPT_PATH"
deployment_paths
verify_release "$SENTINEL_RELEASE_ROOT"

show_status() {
  local name="$1"
  local pid_file="$2"
  local binary="$3"
  local pid
  pid="$(read_running_pid "$pid_file" "$binary" || true)"
  if [[ -n "$pid" ]]; then
    echo "$name: running (PID $pid)"
  else
    echo "$name: stopped"
  fi
}

show_status "Rust application" "$SENTINEL_RUNTIME_PATH/app.pid" \
  "$SENTINEL_RELEASE_ROOT/bin/sentinel-monitor"
show_status "MediaMTX" "$SENTINEL_RUNTIME_PATH/mediamtx.pid" \
  "$SENTINEL_RELEASE_ROOT/bin/mediamtx"
curl -fsS "${SENTINEL_READY_URL:-http://127.0.0.1:8080/health/ready}" || true
echo
