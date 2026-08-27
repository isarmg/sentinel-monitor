#!/usr/bin/env bash
set -euo pipefail

RUNTIME_DIR="${SENTINEL_RUNTIME_DIR:-/mnt/c/Users/micro/sentinel-runtime}"

stop_pid() {
  local name="$1"
  local pid_file="$2"
  if [[ -f "$pid_file" ]]; then
    local pid
    pid="$(cat "$pid_file")"
    if kill -0 "$pid" 2>/dev/null; then
      kill "$pid"
      for _ in {1..20}; do
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.25
      done
    fi
    rm -f "$pid_file"
    echo "$name stopped"
  fi
}

stop_pid "Rust application" "$RUNTIME_DIR/app.pid"
stop_pid "MediaMTX" "$RUNTIME_DIR/mediamtx.pid"

