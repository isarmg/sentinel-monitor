#!/usr/bin/env bash
set -u

RUNTIME_DIR="${SENTINEL_RUNTIME_DIR:-/mnt/c/Users/micro/sentinel-runtime}"

show_status() {
  local name="$1"
  local pid_file="$2"
  if [[ -f "$pid_file" ]] && kill -0 "$(cat "$pid_file")" 2>/dev/null; then
    echo "$name: running (PID $(cat "$pid_file"))"
  else
    echo "$name: stopped"
  fi
}

show_status "Rust application" "$RUNTIME_DIR/app.pid"
show_status "MediaMTX" "$RUNTIME_DIR/mediamtx.pid"
pg_isready || true
curl -fsS http://127.0.0.1:8080/health/ready || true
echo

