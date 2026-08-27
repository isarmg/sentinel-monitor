#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNTIME_DIR="${SENTINEL_RUNTIME_DIR:-/mnt/c/Users/micro/sentinel-runtime}"
export CARGO_TARGET_DIR="${SENTINEL_BUILD_TARGET:-/dev/shm/sentinel-monitor-build}"
CARGO_COMMAND="${CARGO_BIN:-$HOME/.cargo/bin/cargo}"

if [[ ! -x "$CARGO_COMMAND" ]]; then
  CARGO_COMMAND="$(command -v cargo || true)"
fi
if [[ -z "$CARGO_COMMAND" ]]; then
  echo "Rust/Cargo is not installed" >&2
  exit 1
fi

if ! command -v node >/dev/null 2>&1; then
  NODE_BINARY="$(find "$HOME/.nvm/versions/node" -maxdepth 3 -type f -name node -print -quit 2>/dev/null || true)"
  if [[ -z "$NODE_BINARY" ]]; then
    echo "Node.js is not installed" >&2
    exit 1
  fi
  export PATH="$(dirname "$NODE_BINARY"):$PATH"
fi

mkdir -p "$CARGO_TARGET_DIR" "$RUNTIME_DIR/bin"

cd "$ROOT_DIR/web"
npm install
npm run build

cd "$ROOT_DIR"
"$CARGO_COMMAND" build --release
install -m 0755 "$CARGO_TARGET_DIR/release/sentinel-monitor" "$RUNTIME_DIR/bin/sentinel-monitor"

echo "Native build completed: $RUNTIME_DIR/bin/sentinel-monitor"
