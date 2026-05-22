#!/usr/bin/env bash
set -euo pipefail

REPO="ZR233/mai-team"
SERVER_VERSION="latest"
DRY_RUN="false"
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
Usage: update-mai-server-ubuntu-24.04.sh [--version mai-server-vX.Y.Z] [--bind-addr HOST:PORT] [--dry-run]
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

if [[ "$SERVER_VERSION" == "latest" ]]; then
  DOWNLOAD_URL="https://github.com/$REPO/releases/latest/download/$ASSET"
else
  DOWNLOAD_URL="https://github.com/$REPO/releases/download/$SERVER_VERSION/$ASSET"
fi

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

run() {
  if [[ "$DRY_RUN" == "true" ]]; then
    printf 'DRY RUN:'
    printf ' %q' "$@"
    printf '\n'
  else
    "$@"
  fi
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
check_docker_access
run install -d -m 0755 "$ENV_DIR" "$DATA_DIR" "$BIN_DIR"

if [[ "$DRY_RUN" != "true" ]]; then
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
