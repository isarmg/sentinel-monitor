#!/usr/bin/env bash

readonly SENTINEL_PRODUCT="sentinel-monitor"
readonly SENTINEL_VERSION="0.2.0"
readonly SENTINEL_STATIC_FORMAT="sentinel-static-layout-v1"
readonly SENTINEL_RELEASE_FORMAT="sentinel-release-v1"

die() {
  echo "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "Missing required command: $1"
}

validate_absolute_path() {
  local path="$1"
  local label="$2"
  [[ "$path" == /* ]] || die "$label must be an absolute path"
  [[ "$path" != *$'\n'* && "$path" != *$'\r'* ]] || die "$label contains a newline"
  [[ "$path" =~ ^/[A-Za-z0-9._/-]+$ ]] || die "$label contains non-portable characters"
  [[ "/$path/" != *'/../'* && "/$path/" != *'/./'* && "$path" != *'//'* ]] ||
    die "$label must be lexically normalized"
}

assert_no_symlink_components() {
  local path="$1"
  local label="$2"
  validate_absolute_path "$path" "$label"
  local current=""
  local component
  local -a components
  IFS='/' read -r -a components <<<"${path#/}"
  for component in "${components[@]}"; do
    [[ -n "$component" ]] || continue
    current="${current}/${component}"
    [[ ! -L "$current" ]] || die "$label must not traverse a symbolic link: $current"
    if [[ -e "$current" ]]; then
      [[ -d "$current" ]] || die "$label has a non-directory component: $current"
    fi
  done
}

ensure_directory() {
  local path="$1"
  local mode="$2"
  local label="$3"
  validate_absolute_path "$path" "$label"
  local current=""
  local component
  local -a components
  IFS='/' read -r -a components <<<"${path#/}"
  for component in "${components[@]}"; do
    [[ -n "$component" ]] || continue
    current="${current}/${component}"
    [[ ! -L "$current" ]] || die "$label must not traverse a symbolic link: $current"
    if [[ -e "$current" ]]; then
      [[ -d "$current" ]] || die "$label has a non-directory component: $current"
    else
      mkdir -- "$current"
      chmod "$mode" -- "$current"
    fi
  done
  [[ "$(stat -c '%a' -- "$path")" == "$mode" ]] ||
    die "$label must have mode 0$mode: $path"
}

assert_directory() {
  local path="$1"
  local label="$2"
  assert_no_symlink_components "$path" "$label"
  [[ -d "$path" ]] || die "$label is missing: $path"
}

assert_private_directory() {
  local path="$1"
  local label="$2"
  assert_directory "$path" "$label"
  [[ "$(stat -c '%a' -- "$path")" == "700" ]] ||
    die "$label must have mode 0700: $path"
}

assert_regular_file() {
  local path="$1"
  local label="$2"
  if [[ "$path" == /* ]]; then
    assert_no_symlink_components "$(dirname "$path")" "$label parent"
  fi
  [[ ! -L "$path" && -f "$path" ]] || die "$label must be a regular non-symlink file: $path"
  [[ "$(stat -c '%h' -- "$path")" == "1" ]] || die "$label must not be hard linked: $path"
}

assert_private_file() {
  local path="$1"
  local label="$2"
  assert_regular_file "$path" "$label"
  [[ "$(stat -c '%a' -- "$path")" == "600" ]] || die "$label must have mode 0600: $path"
}

portable_relative_path() {
  local path="$1"
  [[ -n "$path" && "$path" != /* && "$path" != *'//'* ]] || return 1
  [[ "/$path/" != *'/../'* && "/$path/" != *'/./'* ]] || return 1
  [[ "$path" =~ ^[A-Za-z0-9._/-]+$ ]]
}

write_static_manifest() {
  local root="$1"
  local output="$2"
  assert_directory "$root" "static build directory"
  local temporary="${output}.tmp.$$"
  (
    umask 077
    {
      echo "format=$SENTINEL_STATIC_FORMAT"
      echo "application=$SENTINEL_PRODUCT"
      echo "application_version=$SENTINEL_VERSION"
      while IFS= read -r -d '' relative; do
        portable_relative_path "$relative" || die "Static layout contains a non-portable path"
        local path="$root/$relative"
        [[ ! -L "$path" ]] || die "Static layout contains a symbolic link: $relative"
        if [[ -d "$path" ]]; then
          echo "directory $relative"
        elif [[ -f "$path" ]]; then
          [[ "$(stat -c '%h' -- "$path")" == "1" ]] ||
            die "Static layout contains a hard-linked file: $relative"
          local size
          local digest
          size="$(stat -c '%s' -- "$path")"
          digest="$(sha256sum -- "$path" | awk '{print $1}')"
          echo "file $size $digest $relative"
        else
          die "Static layout contains a special file: $relative"
        fi
      done < <(find -P "$root" -mindepth 1 -printf '%P\0' | LC_ALL=C sort -z)
    } >"$temporary"
  )
  mv -T -- "$temporary" "$output"
}

write_release_manifest() {
  local root="$1"
  local output="$2"
  assert_directory "$root" "release staging directory"
  local temporary="${output}.tmp.$$"
  (
    umask 077
    {
      echo "format=$SENTINEL_RELEASE_FORMAT"
      echo "application=$SENTINEL_PRODUCT"
      echo "application_version=$SENTINEL_VERSION"
      while IFS= read -r -d '' relative; do
        [[ "$relative" != "RELEASE-MANIFEST" ]] || continue
        portable_relative_path "$relative" || die "Release contains a non-portable path"
        local path="$root/$relative"
        [[ ! -L "$path" ]] || die "Release contains a symbolic link: $relative"
        local mode
        mode="$(stat -c '%a' -- "$path")"
        if [[ -d "$path" ]]; then
          echo "directory $mode $relative"
        elif [[ -f "$path" ]]; then
          [[ "$(stat -c '%h' -- "$path")" == "1" ]] ||
            die "Release contains a hard-linked file: $relative"
          local size
          local digest
          size="$(stat -c '%s' -- "$path")"
          digest="$(sha256sum -- "$path" | awk '{print $1}')"
          echo "file $mode $size $digest $relative"
        else
          die "Release contains a special file: $relative"
        fi
      done < <(find -P "$root" -mindepth 1 -printf '%P\0' | LC_ALL=C sort -z)
    } >"$temporary"
  )
  mv -T -- "$temporary" "$output"
}

verify_release() {
  local root="$1"
  assert_directory "$root" "release directory"
  [[ "$(stat -c '%a' -- "$root")" == "555" ]] || die "Release root must have mode 0555: $root"
  [[ -z "$(find -P "$root" -mindepth 1 -perm /222 -print -quit)" ]] ||
    die "Release content must not have writable permission bits: $root"

  local directory
  for directory in bin config native web; do
    assert_directory "$root/$directory" "release $directory directory"
    [[ "$(stat -c '%a' -- "$root/$directory")" == "555" ]] ||
      die "Release $directory directory must have mode 0555"
  done
  local executable
  for executable in \
    bin/sentinel-monitor \
    bin/mediamtx \
    native/common.sh \
    native/bootstrap.sh \
    native/start.sh \
    native/status.sh \
    native/stop.sh; do
    assert_regular_file "$root/$executable" "release executable $executable"
    [[ "$(stat -c '%a' -- "$root/$executable")" == "555" ]] ||
      die "Release executable must have mode 0555: $executable"
  done
  local configuration
  for configuration in config/mediamtx.yml config/mediamtx.lock; do
    assert_regular_file "$root/$configuration" "release configuration $configuration"
    [[ "$(stat -c '%a' -- "$root/$configuration")" == "444" ]] ||
      die "Release configuration must have mode 0444: $configuration"
  done

  local manifest="$root/RELEASE-MANIFEST"
  assert_regular_file "$manifest" "release manifest"
  [[ "$(stat -c '%a' -- "$manifest")" == "444" ]] || die "Release manifest must have mode 0444"
  local generated
  generated="$(mktemp)"
  if ! write_release_manifest "$root" "$generated"; then
    rm -f -- "$generated"
    return 1
  fi
  if ! cmp -s -- "$manifest" "$generated"; then
    rm -f -- "$generated"
    die "Release content does not match RELEASE-MANIFEST: $root"
  fi
  rm -f -- "$generated"
}

resolve_release_context() {
  local script_path="$1"
  local script_directory
  script_directory="$(cd "$(dirname "$script_path")" && pwd -P)"
  SENTINEL_RELEASE_ROOT="$(cd "$script_directory/.." && pwd -P)"
  [[ "$(basename "$SENTINEL_RELEASE_ROOT")" == "$SENTINEL_VERSION" ]] ||
    die "Operational scripts must run from releases/$SENTINEL_VERSION"
  local releases_root
  releases_root="$(dirname "$SENTINEL_RELEASE_ROOT")"
  [[ "$(basename "$releases_root")" == "releases" ]] ||
    die "Operational scripts must run from an immutable releases directory"
  SENTINEL_INSTALL_ROOT="$(dirname "$releases_root")"
  validate_absolute_path "$SENTINEL_INSTALL_ROOT" "install root"
  if [[ -n "${SENTINEL_NATIVE_INSTALL_ROOT:-}" ]]; then
    validate_absolute_path "$SENTINEL_NATIVE_INSTALL_ROOT" "SENTINEL_NATIVE_INSTALL_ROOT"
    [[ "$SENTINEL_NATIVE_INSTALL_ROOT" == "$SENTINEL_INSTALL_ROOT" ]] ||
      die "SENTINEL_NATIVE_INSTALL_ROOT does not match the executing release"
  fi
  readonly SENTINEL_RELEASE_ROOT SENTINEL_INSTALL_ROOT
}

deployment_paths() {
  SENTINEL_CONFIG_DIR="${SENTINEL_NATIVE_CONFIG_DIR:-/etc/isarmg/sentinel-monitor}"
  SENTINEL_STATE_DIR="${SENTINEL_NATIVE_STATE_DIR:-/var/lib/isarmg/sentinel-monitor}"
  SENTINEL_RUNTIME_PATH="${SENTINEL_NATIVE_RUNTIME_DIR:-/run/isarmg/sentinel-monitor}"
  validate_absolute_path "$SENTINEL_CONFIG_DIR" "SENTINEL_NATIVE_CONFIG_DIR"
  validate_absolute_path "$SENTINEL_STATE_DIR" "SENTINEL_NATIVE_STATE_DIR"
  validate_absolute_path "$SENTINEL_RUNTIME_PATH" "SENTINEL_NATIVE_RUNTIME_DIR"
  SENTINEL_ENV_FILE="$SENTINEL_CONFIG_DIR/sentinel-monitor.env"
  # Consumed by the operational scripts that source this file.
  # shellcheck disable=SC2034
  SENTINEL_REVIEW_MARKER="$SENTINEL_CONFIG_DIR/REVIEW-SECRETS-BEFORE-START"
  readonly SENTINEL_CONFIG_DIR SENTINEL_STATE_DIR SENTINEL_RUNTIME_PATH
  # shellcheck disable=SC2034
  readonly SENTINEL_ENV_FILE SENTINEL_REVIEW_MARKER
}

load_deployment_env() {
  assert_private_directory "$SENTINEL_CONFIG_DIR" "Sentinel configuration directory"
  assert_private_file "$SENTINEL_ENV_FILE" "Sentinel environment file"
  set -a
  # shellcheck disable=SC1090
  source "$SENTINEL_ENV_FILE"
  set +a
}

acquire_native_operation_lock() {
  assert_private_directory "$SENTINEL_RUNTIME_PATH" "Sentinel runtime directory"
  local path="$SENTINEL_RUNTIME_PATH/operations.lock"
  ensure_lock_file "$path" "native operation lock"
  # File descriptor 9 is reserved by native operational scripts. Long-lived
  # child processes must close it explicitly before exec.
  exec 9<>"$path"
  flock --exclusive --nonblock 9 ||
    die "another Sentinel start or stop operation is active"
}

require_runtime_contract() {
  [[ "${DATABASE_URL:-}" == "sqlite://$SENTINEL_STATE_DIR/db/app.db" ]] ||
    die "DATABASE_URL must target the new 0.2.0 state directory"
  [[ "${STATIC_DIR:-}" == "$SENTINEL_RELEASE_ROOT/web" ]] ||
    die "STATIC_DIR must target the executing immutable release"
  [[ "${MEDIAMTX_CONFIG:-}" == "$SENTINEL_RELEASE_ROOT/config/mediamtx.yml" ]] ||
    die "MEDIAMTX_CONFIG must target the executing immutable release"
  [[ "${MEDIAMTX_CONTRACT:-}" == "$SENTINEL_RELEASE_ROOT/config/mediamtx.lock" ]] ||
    die "MEDIAMTX_CONTRACT must target the executing immutable release"
  [[ "${MEDIAMTX_BINARY:-}" == "$SENTINEL_RELEASE_ROOT/bin/mediamtx" ]] ||
    die "MEDIAMTX_BINARY must target the executing immutable release"
  [[ "${RECORDINGS_DIR:-}" == "$SENTINEL_STATE_DIR/recordings" ]] ||
    die "RECORDINGS_DIR must target the new 0.2.0 state directory"
  [[ "${SENTINEL_RUNTIME_DIR:-}" == "$SENTINEL_RUNTIME_PATH" ]] ||
    die "SENTINEL_RUNTIME_DIR must target the configured runtime directory"
  [[ -n "${APP_JWT_SECRET:-}" && ${#APP_JWT_SECRET} -ge 32 ]] ||
    die "APP_JWT_SECRET must contain at least 32 characters"
  [[ -n "${CREDENTIALS_KEY:-}" ]] || die "CREDENTIALS_KEY is required"
  [[ -n "${BOOTSTRAP_ADMIN_PASSWORD:-}" ]] || die "BOOTSTRAP_ADMIN_PASSWORD is required"
}

ensure_lock_file() {
  local path="$1"
  local label="$2"
  if [[ -e "$path" || -L "$path" ]]; then
    assert_private_file "$path" "$label"
    return
  fi
  (umask 077; set -o noclobber; : >"$path") || die "Failed to create $label"
  chmod 600 -- "$path"
  assert_private_file "$path" "$label"
}

read_running_pid() {
  local path="$1"
  local expected_binary="${2:-}"
  if [[ ! -e "$path" && ! -L "$path" ]]; then
    return 1
  fi
  assert_private_file "$path" "PID file"
  local pid
  pid="$(<"$path")"
  [[ "$pid" =~ ^[1-9][0-9]*$ ]] || die "PID file is invalid: $path"
  if ! kill -0 "$pid" 2>/dev/null; then
    return 1
  fi
  if [[ -n "$expected_binary" && -e "/proc/$pid/exe" ]]; then
    local actual
    actual="$(readlink -f -- "/proc/$pid/exe")"
    if [[ "$actual" != "$expected_binary" ]]; then
      local matches_script=false
      local argument
      while IFS= read -r -d '' argument; do
        if [[ "$argument" == "$expected_binary" ]]; then
          matches_script=true
        fi
      done <"/proc/$pid/cmdline"
      [[ "$matches_script" == true ]] ||
        die "PID $pid does not belong to the expected release binary"
    fi
  fi
  echo "$pid"
}

write_pid_file() {
  local path="$1"
  local pid="$2"
  if [[ -e "$path" || -L "$path" ]]; then
    assert_private_file "$path" "PID file"
  fi
  local temporary
  temporary="$(mktemp "${path}.tmp.XXXXXX")"
  (umask 077; printf '%s\n' "$pid" >"$temporary")
  chmod 600 -- "$temporary"
  assert_private_file "$temporary" "temporary PID file"
  mv -T -- "$temporary" "$path"
}
