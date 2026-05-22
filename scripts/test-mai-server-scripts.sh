#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd -- "$SCRIPT_DIR/.." && pwd)"

assert_contains() {
  local haystack="$1"
  local needle="$2"
  if ! grep -Fq -- "$needle" <<<"$haystack"; then
    printf 'expected output to contain: %s\n' "$needle" >&2
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

install_output="$("$INSTALL_SCRIPT" --dry-run)"
update_output="$("$UPDATE_SCRIPT" --dry-run)"

for output in "$install_output" "$update_output"; do
  assert_contains "$output" "DRY RUN: host check would require Ubuntu 22.04 or 24.04 x86_64"
  assert_contains "$output" "DRY RUN: write /etc/systemd/system/mai-server.service:"
  assert_contains "$output" "EnvironmentFile=/etc/mai-server/mai-server.env"
  assert_contains "$output" "ExecStart=/opt/mai-server/mai-server --data-path /var/lib/mai-server"
  assert_contains "$output" "DRY RUN: systemctl daemon-reload"
  assert_contains "$output" "DRY RUN: systemctl enable --now mai-server"
  assert_line_before \
    "$output" \
    "DRY RUN: write /etc/systemd/system/mai-server.service:" \
    "DRY RUN: systemctl daemon-reload"
done

assert_contains "$update_output" "DRY RUN: systemctl restart mai-server"
assert_line_before \
  "$update_output" \
  "DRY RUN: systemctl daemon-reload" \
  "DRY RUN: systemctl restart mai-server"

printf 'mai server script tests passed\n'
