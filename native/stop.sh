#!/usr/bin/env bash
set -euo pipefail

SCRIPT_PATH="${BASH_SOURCE[0]}"
# shellcheck source=/dev/null
source "$(cd "$(dirname "$SCRIPT_PATH")" && pwd -P)/common.sh"
resolve_release_context "$SCRIPT_PATH"
deployment_paths
verify_release "$SENTINEL_RELEASE_ROOT"
require_command flock
acquire_native_operation_lock

stop_pid() {
  local name="$1"
  local pid_file="$2"
  local binary="$3"
  local pid
  pid="$(read_running_pid "$pid_file" "$binary" || true)"
  if [[ -z "$pid" ]]; then
    if [[ -e "$pid_file" || -L "$pid_file" ]]; then
      assert_private_file "$pid_file" "$name PID file"
      rm -- "$pid_file"
    fi
    echo "$name: already stopped"
    return
  fi
  kill "$pid"
  for _ in {1..40}; do
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.25
  done
  kill -0 "$pid" 2>/dev/null && die "$name did not stop within 10 seconds"
  if [[ -e "$pid_file" || -L "$pid_file" ]]; then
    assert_private_file "$pid_file" "$name PID file"
    rm -- "$pid_file"
  fi
  echo "$name stopped"
}

# Keep the established shutdown order: application/database/runtime locks first,
# then the MediaMTX companion lock.
stop_pid "Rust application" "$SENTINEL_RUNTIME_PATH/app.pid" \
  "$SENTINEL_RELEASE_ROOT/bin/sentinel-monitor"
stop_pid "MediaMTX" "$SENTINEL_RUNTIME_PATH/mediamtx.pid" \
  "$SENTINEL_RELEASE_ROOT/bin/mediamtx"
