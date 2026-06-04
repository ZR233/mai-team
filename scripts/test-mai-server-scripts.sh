#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd -- "$SCRIPT_DIR/.." && pwd)"
release_fixture="$(mktemp)"
wrong_version_output="$(mktemp)"
trap 'rm -f "$release_fixture" "$wrong_version_output"' EXIT

cat > "$release_fixture" <<'JSON'
[
  { "tag_name": "mai-relay-v0.1.8" },
  { "tag_name": "mai-server-v0.1.8", "draft": true },
  { "tag_name": "mai-server-v0.1.7" }
]
JSON

assert_contains() {
  local haystack="$1"
  local needle="$2"
  if ! grep -Fq -- "$needle" <<<"$haystack"; then
    printf 'expected output to contain: %s\n' "$needle" >&2
    exit 1
  fi
}

assert_not_contains() {
  local haystack="$1"
  local needle="$2"
  if grep -Fq -- "$needle" <<<"$haystack"; then
    printf 'expected output not to contain: %s\n' "$needle" >&2
    exit 1
  fi
}

line_number() {
  local haystack="$1"
  local needle="$2"
  grep -nF -- "$needle" <<<"$haystack" | head -n1 | cut -d: -f1 || true
}

assert_line_before() {
  local haystack="$1"
  local first="$2"
  local second="$3"
  local first_line
  local second_line
  first_line="$(line_number "$haystack" "$first")"
  second_line="$(line_number "$haystack" "$second")"

  if [[ -z "$first_line" || -z "$second_line" || "$first_line" -ge "$second_line" ]]; then
    printf 'expected "%s" to appear before "%s"\n' "$first" "$second" >&2
    exit 1
  fi
}

INSTALL_SCRIPT="$ROOT_DIR/scripts/install-mai-server-ubuntu-24.04.sh"
UPDATE_SCRIPT="$ROOT_DIR/scripts/update-mai-server-ubuntu-24.04.sh"

bash -n "$INSTALL_SCRIPT"
bash -n "$UPDATE_SCRIPT"

install_output="$(MAI_RELEASES_JSON_FILE="$release_fixture" "$INSTALL_SCRIPT" --dry-run)"
update_output="$(MAI_RELEASES_JSON_FILE="$release_fixture" "$UPDATE_SCRIPT" --dry-run)"
source_install_output="$("$INSTALL_SCRIPT" --dry-run --source-dir "$ROOT_DIR")"
source_update_output="$("$UPDATE_SCRIPT" --dry-run --source-dir "$ROOT_DIR")"
sudo_source_install_output="$(SUDO_USER=mai-build-user "$INSTALL_SCRIPT" --dry-run --source-dir "$ROOT_DIR")"
sudo_source_update_output="$(SUDO_USER=mai-build-user "$UPDATE_SCRIPT" --dry-run --source-dir "$ROOT_DIR")"

for output in "$install_output" "$update_output"; do
  assert_contains "$output" "DRY RUN: host check would require Ubuntu 22.04 or 24.04 x86_64"
  assert_contains "$output" "DRY RUN: resolve latest mai-server release from https://api.github.com/repos/ZR233/mai-team/releases?per_page=100"
  assert_contains "$output" "DRY RUN: selected latest mai-server release mai-server-v0.1.7"
  assert_contains "$output" "DRY RUN: download https://github.com/ZR233/mai-team/releases/download/mai-server-v0.1.7/mai-server-x86_64-unknown-linux-gnu.tar.gz"
  assert_not_contains "$output" "releases/latest/download"
  assert_contains "$output" "DRY RUN: configure mai-server access to Docker socket group"
  assert_contains "$output" "DRY RUN: write /etc/systemd/system/mai-server.service:"
  assert_contains "$output" "EnvironmentFile=/etc/mai-server/mai-server.env"
  assert_contains "$output" "ExecStart=/opt/mai-server/mai-server --data-path /var/lib/mai-server"
  assert_contains "$output" "DRY RUN: systemctl daemon-reload"
  assert_contains "$output" "DRY RUN: systemctl enable --now mai-server"
  assert_line_before \
    "$output" \
    "DRY RUN: write /etc/systemd/system/mai-server.service:" \
    "DRY RUN: systemctl daemon-reload"
  assert_line_before \
    "$output" \
    "DRY RUN: configure mai-server access to Docker socket group" \
    "DRY RUN: check Docker daemon and mai-server user access to Docker socket"
done

assert_contains "$update_output" "DRY RUN: systemctl restart mai-server"
assert_line_before \
  "$update_output" \
  "DRY RUN: systemctl daemon-reload" \
  "DRY RUN: systemctl restart mai-server"

for output in "$source_install_output" "$source_update_output"; do
  assert_contains "$output" "DRY RUN: build mai-server from source $ROOT_DIR as $(id -un)"
  assert_contains "$output" "DRY RUN: cargo build --release -p mai-server"
  assert_contains "$output" "DRY RUN: source build environment loads Cargo and Node.js/npm from the build user"
  assert_contains "$output" "DRY RUN: install built mai-server from $ROOT_DIR/target/release/mai-server to /opt/mai-server/mai-server"
  assert_line_before \
    "$output" \
    "DRY RUN: cargo build --release -p mai-server" \
    "DRY RUN: install built mai-server from $ROOT_DIR/target/release/mai-server to /opt/mai-server/mai-server"
  assert_not_contains "$output" "DRY RUN: resolve latest mai-server release"
  assert_not_contains "$output" "DRY RUN: download https://github.com/ZR233/mai-team/releases/download"
done

assert_contains "$source_update_output" "DRY RUN: systemctl restart mai-server"
for output in "$sudo_source_install_output" "$sudo_source_update_output"; do
  assert_contains "$output" "DRY RUN: build mai-server from source $ROOT_DIR as mai-build-user"
  assert_contains "$output" "DRY RUN: sudo -u mai-build-user -H bash -lc"
  assert_contains "$output" "source build environment loads Cargo and Node.js/npm from the build user"
  assert_contains "$output" "cargo build --release -p mai-server"
  assert_not_contains "$output" "DRY RUN: cargo build --release -p mai-server"
  assert_line_before \
    "$output" \
    "DRY RUN: sudo -u mai-build-user -H bash -lc" \
    "DRY RUN: install built mai-server from $ROOT_DIR/target/release/mai-server to /opt/mai-server/mai-server"
done

if MAI_RELEASES_JSON_FILE="$release_fixture" "$UPDATE_SCRIPT" --dry-run --version mai-relay-v0.1.8 >"$wrong_version_output" 2>&1; then
  printf 'expected server update script to reject mai-relay version tag\n' >&2
  exit 1
fi
assert_contains "$(< "$wrong_version_output")" "--version for mai-server must start with mai-server-v"

printf 'mai server script tests passed\n'
