#!/usr/bin/env bash
set -euo pipefail

REPO="ZR233/mai-team"
RELAY_VERSION="latest"
ROTATE_TOKEN="false"
DRY_RUN="false"
PUBLIC_URL=""

usage() {
  cat <<'USAGE'
Usage: update-mai-relay-ubuntu-24.04.sh [--version mai-relay-vX.Y.Z] [--public-url URL] [--rotate-token] [--dry-run]
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)
      RELAY_VERSION="${2:?--version requires a value}"
      shift 2
      ;;
    --public-url)
      PUBLIC_URL="${2:?--public-url requires a value}"
      shift 2
      ;;
    --rotate-token)
      ROTATE_TOKEN="true"
      shift
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

if [[ "$(uname -m)" != "x86_64" && "$DRY_RUN" != "true" ]]; then
  echo "mai-relay updater currently supports only x86_64 Ubuntu 24.04" >&2
  exit 1
fi

if [[ -r /etc/os-release ]]; then
  . /etc/os-release
  if [[ "${ID:-}" != "ubuntu" || "${VERSION_ID:-}" != "24.04" ]]; then
    if [[ "$DRY_RUN" == "true" ]]; then
      echo "DRY RUN: host check would require Ubuntu 24.04 x86_64"
    else
      echo "mai-relay updater currently supports only Ubuntu 24.04" >&2
      exit 1
    fi
  fi
fi

if [[ $EUID -ne 0 && "$DRY_RUN" != "true" ]]; then
  echo "run as root or use sudo" >&2
  exit 1
fi

ENV_DIR="/etc/mai-relay"
DATA_DIR="/var/lib/mai-relay"
BIN_DIR="/opt/mai-relay"
ENV_FILE="$ENV_DIR/mai-relay.env"
BIN_PATH="$BIN_DIR/mai-relay"
LEGACY_BIN_PATH="/usr/local/bin/mai-relay"
SERVICE_FILE="/etc/systemd/system/mai-relay.service"
ASSET="mai-relay-x86_64-unknown-linux-gnu.tar.gz"

if [[ "$RELAY_VERSION" == "latest" ]]; then
  DOWNLOAD_URL="https://github.com/$REPO/releases/latest/download/$ASSET"
else
  DOWNLOAD_URL="https://github.com/$REPO/releases/download/$RELAY_VERSION/$ASSET"
fi

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

existing_token="$(env_value MAI_RELAY_TOKEN "$ENV_FILE")"
existing_public_url="$(env_value MAI_RELAY_PUBLIC_URL "$ENV_FILE")"
existing_bind_addr="$(env_value MAI_RELAY_BIND_ADDR "$ENV_FILE")"
existing_db_path="$(env_value MAI_RELAY_DB_PATH "$ENV_FILE")"

if [[ "$ROTATE_TOKEN" == "true" || -z "$existing_token" ]]; then
  token="$(openssl rand -hex 32)"
else
  token="$existing_token"
fi

if [[ -z "$PUBLIC_URL" ]]; then
  PUBLIC_URL="${existing_public_url:-http://127.0.0.1:8090}"
fi

BIND_ADDR="${existing_bind_addr:-0.0.0.0:8090}"
DB_PATH="${existing_db_path:-$DATA_DIR/mai-relay.sqlite3}"

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

run useradd --system --home "$DATA_DIR" --shell /usr/sbin/nologin mai-relay 2>/dev/null || true
run install -d -m 0755 "$ENV_DIR" "$DATA_DIR" "$BIN_DIR"

if [[ "$DRY_RUN" != "true" ]]; then
  curl -fsSL "$DOWNLOAD_URL" -o "$tmpdir/$ASSET"
  tar -xzf "$tmpdir/$ASSET" -C "$tmpdir"
  install -m 0755 "$tmpdir/mai-relay" "$BIN_PATH"
  chown -R mai-relay:mai-relay "$BIN_DIR"
  ln -sfn "$BIN_PATH" "$LEGACY_BIN_PATH"
else
  echo "DRY RUN: download $DOWNLOAD_URL"
  echo "DRY RUN: install extracted mai-relay to $BIN_PATH"
  echo "DRY RUN: chown -R mai-relay:mai-relay $BIN_DIR"
  echo "DRY RUN: ln -sfn $BIN_PATH $LEGACY_BIN_PATH"
fi

env_content="$(cat <<ENV
MAI_RELAY_TOKEN='$token'
MAI_RELAY_PUBLIC_URL='$PUBLIC_URL'
MAI_RELAY_BIND_ADDR='$BIND_ADDR'
MAI_RELAY_DB_PATH='$DB_PATH'
ENV
)"

if [[ "$DRY_RUN" != "true" ]]; then
  umask 077
  printf '%s\n' "$env_content" > "$ENV_FILE"
else
  echo "DRY RUN: write $ENV_FILE:"
  printf '%s\n' "$env_content"
fi

service_content="$(cat <<SERVICE
[Unit]
Description=Mai Relay
After=network-online.target
Wants=network-online.target

[Service]
User=mai-relay
Group=mai-relay
EnvironmentFile=$ENV_FILE
ExecStart=$BIN_PATH
Restart=always
RestartSec=5
WorkingDirectory=$DATA_DIR

[Install]
WantedBy=multi-user.target
SERVICE
)"

if [[ "$DRY_RUN" != "true" ]]; then
  printf '%s\n' "$service_content" > "$tmpdir/mai-relay.service"
  install -m 0644 "$tmpdir/mai-relay.service" "$SERVICE_FILE"
  chown -R mai-relay:mai-relay "$DATA_DIR"
  systemctl daemon-reload
  systemctl enable --now mai-relay
  systemctl restart mai-relay
else
  echo "DRY RUN: write $SERVICE_FILE:"
  printf '%s\n' "$service_content"
  echo "DRY RUN: systemctl daemon-reload"
  echo "DRY RUN: systemctl enable --now mai-relay"
  echo "DRY RUN: systemctl restart mai-relay"
fi

cat <<INFO
mai-relay updated.
Relay URL: $PUBLIC_URL
Node ID: mai-server
Relay token: $token
Configure these values in mai-server Settings > GitHub App.
INFO
