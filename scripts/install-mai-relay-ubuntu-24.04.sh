#!/usr/bin/env bash
set -euo pipefail

REPO="ZR233/mai-team"
RELAY_VERSION="latest"
ROTATE_TOKEN="false"
DRY_RUN="false"
BIND_ADDR="0.0.0.0:8090"
PUBLIC_URL=""

usage() {
  cat <<'USAGE'
Usage: install-mai-relay-ubuntu-24.04.sh [--version vX.Y.Z] [--public-url URL] [--rotate-token] [--dry-run]
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
  echo "mai-relay installer currently supports only x86_64 Ubuntu 24.04" >&2
  exit 1
fi

if [[ -r /etc/os-release ]]; then
  . /etc/os-release
  if [[ "${ID:-}" != "ubuntu" || "${VERSION_ID:-}" != "24.04" ]]; then
    if [[ "$DRY_RUN" == "true" ]]; then
      echo "DRY RUN: host check would require Ubuntu 24.04 x86_64"
    else
    echo "mai-relay installer currently supports only Ubuntu 24.04" >&2
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
ENV_FILE="$ENV_DIR/mai-relay.env"
BIN_PATH="/usr/local/bin/mai-relay"
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

existing_token=""
if [[ -r "$ENV_FILE" ]]; then
  existing_token="$(sed -n "s/^MAI_RELAY_TOKEN='\\(.*\\)'$/\\1/p;s/^MAI_RELAY_TOKEN=\\(.*\\)$/\\1/p" "$ENV_FILE" | tail -n1)"
fi

if [[ "$ROTATE_TOKEN" == "true" || -z "$existing_token" ]]; then
  token="$(openssl rand -hex 32)"
else
  token="$existing_token"
fi

if [[ -z "$PUBLIC_URL" ]]; then
  PUBLIC_URL="http://127.0.0.1:8090"
fi

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

run install -d -m 0755 "$ENV_DIR" "$DATA_DIR"
run useradd --system --home "$DATA_DIR" --shell /usr/sbin/nologin mai-relay 2>/dev/null || true

if [[ "$DRY_RUN" != "true" ]]; then
  curl -fsSL "$DOWNLOAD_URL" -o "$tmpdir/$ASSET"
  tar -xzf "$tmpdir/$ASSET" -C "$tmpdir"
  install -m 0755 "$tmpdir/mai-relay" "$BIN_PATH"
else
  echo "DRY RUN: download $DOWNLOAD_URL"
fi

env_content="$(cat <<ENV
MAI_RELAY_TOKEN='$token'
MAI_RELAY_PUBLIC_URL='$PUBLIC_URL'
MAI_RELAY_BIND_ADDR='$BIND_ADDR'
MAI_RELAY_DB_PATH='$DATA_DIR/mai-relay.sqlite3'
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
  printf '%s\n' "$service_content" > "$SERVICE_FILE"
  chown -R mai-relay:mai-relay "$DATA_DIR"
  systemctl daemon-reload
  systemctl enable --now mai-relay
else
  echo "DRY RUN: write $SERVICE_FILE:"
  printf '%s\n' "$service_content"
  echo "DRY RUN: systemctl enable --now mai-relay"
fi

cat <<INFO
mai-relay installed.
Relay URL: $PUBLIC_URL
Node ID: mai-server
Relay token: $token
Configure these values in mai-server Settings > GitHub App.
INFO
