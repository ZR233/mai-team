#!/usr/bin/env bash
set -euo pipefail

REPO="ZR233/mai-team"
SERVER_VERSION="latest"
RELEASE_PACKAGE="mai-server"
RELEASE_TAG_PREFIX="mai-server-v"
RELEASE_API_URL="https://api.github.com/repos/$REPO/releases?per_page=100"
DRY_RUN="false"
SOURCE_DIR=""
BIND_ADDR=""
DEFAULT_BIND_ADDR="0.0.0.0:8080"
DEFAULT_RUST_LOG="mai_server=info,mai_runtime=info,tower_http=info"

ENV_DIR="/etc/mai-server"
DATA_DIR="/var/lib/mai-server"
BIN_DIR="/opt/mai-server"
ENV_FILE="$ENV_DIR/mai-server.env"
BIN_PATH="$BIN_DIR/mai-server"
LEGACY_BIN_PATH="/usr/local/bin/mai-server"
SERVICE_FILE="/etc/systemd/system/mai-server.service"
ASSET="mai-server-x86_64-unknown-linux-gnu.tar.gz"

usage() {
  cat <<'USAGE'
Usage: update-mai-server-ubuntu-24.04.sh [--version mai-server-vX.Y.Z] [--source-dir PATH] [--bind-addr HOST:PORT] [--dry-run]
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)
      SERVER_VERSION="${2:?--version requires a value}"
      shift 2
      ;;
    --bind-addr)
      BIND_ADDR="${2:?--bind-addr requires a value}"
      shift 2
      ;;
    --source-dir)
      SOURCE_DIR="${2:?--source-dir requires a value}"
      shift 2
      ;;
    --dry-run)
      DRY_RUN="true"
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      exit 2
      ;;
  esac
done

check_host() {
  local arch
  arch="$(uname -m)"
  local os_id=""
  local version_id=""

  if [[ -r /etc/os-release ]]; then
    . /etc/os-release
    os_id="${ID:-}"
    version_id="${VERSION_ID:-}"
  fi

  if [[ "$DRY_RUN" == "true" ]]; then
    echo "DRY RUN: host check would require Ubuntu 22.04 or 24.04 x86_64; detected ${os_id:-unknown} ${version_id:-unknown} $arch"
    return 0
  fi

  if [[ "$arch" == "x86_64" && "$os_id" == "ubuntu" && ( "$version_id" == "22.04" || "$version_id" == "24.04" ) ]]; then
    return 0
  fi

  echo "mai-server updater currently supports only Ubuntu 22.04 or 24.04 x86_64" >&2
  exit 1
}

release_index_json() {
  if [[ -n "${MAI_RELEASES_JSON_FILE:-}" ]]; then
    cat "$MAI_RELEASES_JSON_FILE"
    return 0
  fi

  curl -fsSL \
    -H "Accept: application/vnd.github+json" \
    -H "User-Agent: mai-server-updater" \
    "$RELEASE_API_URL"
}

resolve_release_tag() {
  if [[ "$SERVER_VERSION" == "latest" ]]; then
    if [[ "$DRY_RUN" == "true" ]]; then
      echo "DRY RUN: resolve latest $RELEASE_PACKAGE release from $RELEASE_API_URL"
    fi
    local releases
    releases="$(release_index_json)"
    if command -v python3 >/dev/null 2>&1; then
      RELEASE_TAG="$(RELEASE_TAG_PREFIX="$RELEASE_TAG_PREFIX" python3 -c '
import json
import os
import sys

prefix = os.environ["RELEASE_TAG_PREFIX"]
for release in json.load(sys.stdin):
    if release.get("draft") or release.get("prerelease"):
        continue
    tag = release.get("tag_name", "")
    if tag.startswith(prefix):
        print(tag)
        break
' <<<"$releases")"
    else
      set +o pipefail
      RELEASE_TAG="$(
        grep -E -o "\"tag_name\"[[:space:]]*:[[:space:]]*\"${RELEASE_TAG_PREFIX}[^\"]+\"" <<<"$releases" \
          | head -n1 \
          | sed -E "s/.*\"(${RELEASE_TAG_PREFIX}[^\"]+)\".*/\1/"
      )"
      set -o pipefail
    fi
    if [[ -z "$RELEASE_TAG" ]]; then
      echo "no $RELEASE_PACKAGE release found in $RELEASE_API_URL" >&2
      exit 1
    fi
    if [[ "$DRY_RUN" == "true" ]]; then
      echo "DRY RUN: selected latest $RELEASE_PACKAGE release $RELEASE_TAG"
    fi
    return 0
  fi

  if [[ "$SERVER_VERSION" != "$RELEASE_TAG_PREFIX"* ]]; then
    echo "--version for $RELEASE_PACKAGE must start with $RELEASE_TAG_PREFIX" >&2
    exit 2
  fi
  RELEASE_TAG="$SERVER_VERSION"
}

run() {
  if [[ "$DRY_RUN" == "true" ]]; then
    printf 'DRY RUN:'
    printf ' %q' "$@"
    printf '\n'
  else
    "$@"
  fi
}

source_binary_path() {
  printf '%s/target/release/mai-server' "$SOURCE_DIR"
}

source_build_user() {
  if [[ $EUID -eq 0 && -n "${SUDO_USER:-}" && "${SUDO_USER:-}" != "root" ]]; then
    local sudo_user_uid
    if ! sudo_user_uid="$(id -u "$SUDO_USER" 2>/dev/null)"; then
      echo "SUDO_USER=$SUDO_USER does not exist on this host" >&2
      exit 1
    fi
    if [[ -n "${SUDO_UID:-}" && "$sudo_user_uid" != "$SUDO_UID" ]]; then
      echo "SUDO_USER=$SUDO_USER does not match SUDO_UID=$SUDO_UID" >&2
      exit 1
    fi
    printf '%s' "$SUDO_USER"
    return 0
  fi
  if [[ "$DRY_RUN" == "true" && -n "${SUDO_USER:-}" && "${SUDO_USER:-}" != "root" ]]; then
    printf '%s' "$SUDO_USER"
    return 0
  fi
  id -un
}

run_source_build_as_user() {
  local build_user="$1"
  sudo -u "$build_user" -H bash -lc '
if [[ -r "$HOME/.cargo/env" ]]; then
  . "$HOME/.cargo/env"
fi
if [[ -r "$HOME/.nvm/nvm.sh" ]]; then
  export NVM_DIR="$HOME/.nvm"
  . "$NVM_DIR/nvm.sh"
fi
if command -v mise >/dev/null 2>&1; then
  eval "$(mise activate bash)"
fi
if command -v asdf >/dev/null 2>&1; then
  . "$(asdf info asdf-dir)/asdf.sh"
fi
if command -v fnm >/dev/null 2>&1; then
  eval "$(fnm env --use-on-cd --shell bash)"
fi
if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo was not found for user ${USER:-$(id -un)}. Install Rust/Cargo for that user and retry." >&2
  exit 127
fi
if ! command -v npm >/dev/null 2>&1; then
  echo "npm was not found for user ${USER:-$(id -un)}. Install Node.js/npm for that user and retry." >&2
  exit 127
fi
if ! node -e '"'"'const [major, minor] = process.versions.node.split(".").map(Number); process.exit((major === 20 && minor >= 19) || (major === 22 && minor >= 12) || major >= 23 ? 0 : 1);'"'"'; then
  echo "Node.js $(node --version) does not satisfy mai-server web build requirement: ^20.19.0 || >=22.12.0" >&2
  exit 1
fi
cd "$1" && cargo build --release -p mai-server
' bash "$SOURCE_DIR"
}

validate_source_dir() {
  if [[ -z "$SOURCE_DIR" ]]; then
    return 0
  fi
  if [[ ! -f "$SOURCE_DIR/Cargo.toml" ]]; then
    echo "--source-dir must point to the mai-team repository root with Cargo.toml" >&2
    exit 2
  fi
}

build_source_binary() {
  local build_user
  local current_user
  build_user="$(source_build_user)"
  current_user="$(id -un)"
  if [[ "$DRY_RUN" != "true" && "$current_user" == "root" && "$build_user" == "root" ]]; then
    cat >&2 <<'ERROR'
--source-dir builds should not compile as root.
Install Rust/Cargo for your normal user and run this updater through sudo, for example:
  sudo scripts/update-mai-server-ubuntu-24.04.sh --source-dir "$(pwd)"
ERROR
    exit 1
  fi
  if [[ "$DRY_RUN" == "true" ]]; then
    echo "DRY RUN: build mai-server from source $SOURCE_DIR as $build_user"
    echo "DRY RUN: source build environment loads Cargo and Node.js/npm from the build user"
    if [[ "$build_user" != "$current_user" ]]; then
      echo "DRY RUN: sudo -u $build_user -H bash -lc 'source ~/.cargo/env and Node manager env if present; cd $SOURCE_DIR && cargo build --release -p mai-server'"
    else
      echo "DRY RUN: cargo build --release -p mai-server"
    fi
    return 0
  fi

  if [[ "$build_user" != "$current_user" ]]; then
    run_source_build_as_user "$build_user"
  else
    (cd "$SOURCE_DIR" && cargo build --release -p mai-server)
  fi
}

install_source_binary() {
  local built_binary
  built_binary="$(source_binary_path)"
  if [[ "$DRY_RUN" == "true" ]]; then
    echo "DRY RUN: install built mai-server from $built_binary to $BIN_PATH"
    echo "DRY RUN: chown -R mai-server:mai-server $BIN_DIR"
    echo "DRY RUN: ln -sfn $BIN_PATH $LEGACY_BIN_PATH"
    return 0
  fi

  install -m 0755 "$built_binary" "$BIN_PATH"
  chown -R mai-server:mai-server "$BIN_DIR"
  ln -sfn "$BIN_PATH" "$LEGACY_BIN_PATH"
}

env_value() {
  local name="$1"
  local file="$2"
  if [[ ! -r "$file" ]]; then
    return 0
  fi
  sed -n \
    "s/^${name}='\\(.*\\)'$/\\1/p;s/^${name}=\\(.*\\)$/\\1/p" \
    "$file" | tail -n1
}

docker_socket_group() {
  if [[ -S /var/run/docker.sock ]]; then
    stat -c '%G' /var/run/docker.sock
    return 0
  fi
  if getent group docker >/dev/null 2>&1; then
    printf 'docker\n'
    return 0
  fi
  return 1
}

configure_docker_access() {
  if [[ "$DRY_RUN" == "true" ]]; then
    echo "DRY RUN: configure mai-server access to Docker socket group"
    return 0
  fi

  local group
  if ! group="$(docker_socket_group)"; then
    cat >&2 <<'ERROR'
Docker socket group was not found.
Install Docker and confirm /var/run/docker.sock exists before updating mai-server.
ERROR
    exit 1
  fi

  usermod -aG "$group" mai-server
}

check_docker_access() {
  if [[ "$DRY_RUN" == "true" ]]; then
    echo "DRY RUN: check Docker daemon and mai-server user access to Docker socket"
    return 0
  fi

  if ! docker version >/dev/null 2>&1; then
    cat >&2 <<'ERROR'
Docker is not available to root.
Install Docker, confirm the Docker daemon is running, and ensure the mai-server user can access the Docker socket.
ERROR
    exit 1
  fi

  if ! runuser -u mai-server -- docker version >/dev/null 2>&1; then
    cat >&2 <<'ERROR'
Docker is not available to the mai-server system user.
Install Docker if needed, confirm the Docker daemon is running, and add or configure the mai-server user so it can access the Docker socket.
ERROR
    exit 1
  fi
}

server_url() {
  if [[ "$BIND_ADDR" == 0.0.0.0:* ]]; then
    printf 'http://127.0.0.1:%s' "${BIND_ADDR#0.0.0.0:}"
  else
    printf 'http://%s' "$BIND_ADDR"
  fi
}

managed_env_content() {
  cat <<ENV
MAI_BIND_ADDR='$BIND_ADDR'
RUST_LOG='$RUST_LOG_VALUE'
ENV
}

write_env_file() {
  local target="$tmpdir/mai-server.env"
  : > "$target"
  if [[ -r "$ENV_FILE" ]]; then
    grep -Ev '^(MAI_BIND_ADDR|RUST_LOG)=' "$ENV_FILE" > "$target" || true
  fi
  managed_env_content >> "$target"
  install -m 0600 "$target" "$ENV_FILE"
}

check_host

if [[ $EUID -ne 0 && "$DRY_RUN" != "true" ]]; then
  echo "run as root or use sudo" >&2
  exit 1
fi

validate_source_dir
if [[ -z "$SOURCE_DIR" ]]; then
  resolve_release_tag
  DOWNLOAD_URL="https://github.com/$REPO/releases/download/$RELEASE_TAG/$ASSET"
fi

existing_bind_addr="$(env_value MAI_BIND_ADDR "$ENV_FILE")"
existing_rust_log="$(env_value RUST_LOG "$ENV_FILE")"

if [[ -z "$BIND_ADDR" ]]; then
  BIND_ADDR="${existing_bind_addr:-$DEFAULT_BIND_ADDR}"
fi
RUST_LOG_VALUE="${existing_rust_log:-$DEFAULT_RUST_LOG}"

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

run groupadd --system mai-server 2>/dev/null || true
run useradd --system --gid mai-server --home "$DATA_DIR" --shell /usr/sbin/nologin mai-server 2>/dev/null || true
configure_docker_access
check_docker_access
run install -d -m 0755 "$ENV_DIR" "$DATA_DIR" "$BIN_DIR"

if [[ -n "$SOURCE_DIR" ]]; then
  build_source_binary
  install_source_binary
elif [[ "$DRY_RUN" != "true" ]]; then
  curl -fsSL "$DOWNLOAD_URL" -o "$tmpdir/$ASSET"
  tar -xzf "$tmpdir/$ASSET" -C "$tmpdir"
  install -m 0755 "$tmpdir/mai-server" "$BIN_PATH"
  chown -R mai-server:mai-server "$BIN_DIR"
  ln -sfn "$BIN_PATH" "$LEGACY_BIN_PATH"
else
  echo "DRY RUN: download $DOWNLOAD_URL"
  echo "DRY RUN: install extracted mai-server to $BIN_PATH"
  echo "DRY RUN: chown -R mai-server:mai-server $BIN_DIR"
  echo "DRY RUN: ln -sfn $BIN_PATH $LEGACY_BIN_PATH"
fi

if [[ "$DRY_RUN" != "true" ]]; then
  write_env_file
else
  echo "DRY RUN: write $ENV_FILE:"
  managed_env_content
  if [[ -r "$ENV_FILE" ]]; then
    echo "DRY RUN: preserve existing non-managed variables in $ENV_FILE"
  fi
fi

service_content="$(cat <<SERVICE
[Unit]
Description=Mai Server
After=network-online.target
Wants=network-online.target

[Service]
User=mai-server
Group=mai-server
EnvironmentFile=$ENV_FILE
ExecStart=$BIN_PATH --data-path $DATA_DIR
Restart=always
RestartSec=5
WorkingDirectory=$DATA_DIR

[Install]
WantedBy=multi-user.target
SERVICE
)"

if [[ "$DRY_RUN" != "true" ]]; then
  printf '%s\n' "$service_content" > "$tmpdir/mai-server.service"
  install -m 0644 "$tmpdir/mai-server.service" "$SERVICE_FILE"
  chown -R mai-server:mai-server "$DATA_DIR"
  systemctl daemon-reload
  systemctl enable --now mai-server
  systemctl restart mai-server
else
  echo "DRY RUN: write $SERVICE_FILE:"
  printf '%s\n' "$service_content"
  echo "DRY RUN: chown -R mai-server:mai-server $DATA_DIR"
  echo "DRY RUN: systemctl daemon-reload"
  echo "DRY RUN: systemctl enable --now mai-server"
  echo "DRY RUN: systemctl restart mai-server"
fi

SERVER_URL="$(server_url)"

cat <<INFO
mai-server updated.
Server URL: $SERVER_URL
Env file: $ENV_FILE
Data dir: $DATA_DIR
Health check: curl -fsSL $SERVER_URL/health
INFO
