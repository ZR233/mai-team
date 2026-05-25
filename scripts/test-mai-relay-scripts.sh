#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd -- "$SCRIPT_DIR/.." && pwd)"
release_fixture="$(mktemp)"
wrong_version_output="$(mktemp)"
trap 'rm -f "$release_fixture" "$wrong_version_output"' EXIT

cat > "$release_fixture" <<'JSON'
[
  { "tag_name": "mai-server-v0.1.8" },
  { "tag_name": "mai-relay-v0.1.8", "prerelease": true },
  { "tag_name": "mai-relay-v0.1.7" }
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

INSTALL_SCRIPT="$ROOT_DIR/scripts/install-mai-relay-ubuntu-24.04.sh"
UPDATE_SCRIPT="$ROOT_DIR/scripts/update-mai-relay-ubuntu-24.04.sh"

install_output="$(MAI_RELEASES_JSON_FILE="$release_fixture" "$INSTALL_SCRIPT" --dry-run)"
update_output="$(MAI_RELEASES_JSON_FILE="$release_fixture" "$UPDATE_SCRIPT" --dry-run)"

for output in "$install_output" "$update_output"; do
  assert_contains "$output" "DRY RUN: host check would require Ubuntu 22.04 or 24.04 x86_64"
  assert_contains "$output" "DRY RUN: resolve latest mai-relay release from https://api.github.com/repos/ZR233/mai-team/releases?per_page=100"
  assert_contains "$output" "DRY RUN: selected latest mai-relay release mai-relay-v0.1.7"
  assert_contains "$output" "DRY RUN: download https://github.com/ZR233/mai-team/releases/download/mai-relay-v0.1.7/mai-relay-x86_64-unknown-linux-gnu.tar.gz"
  assert_not_contains "$output" "releases/latest/download"
  assert_contains "$output" "DRY RUN: write /etc/systemd/system/mai-relay.service:"
done

assert_contains "$update_output" "DRY RUN: systemctl daemon-reload"
assert_contains "$update_output" "DRY RUN: systemctl restart mai-relay"
assert_line_before \
  "$update_output" \
  "DRY RUN: write /etc/systemd/system/mai-relay.service:" \
  "DRY RUN: systemctl daemon-reload"
assert_line_before \
  "$update_output" \
  "DRY RUN: systemctl daemon-reload" \
  "DRY RUN: systemctl restart mai-relay"

if MAI_RELEASES_JSON_FILE="$release_fixture" "$UPDATE_SCRIPT" --dry-run --version mai-server-v0.1.8 >"$wrong_version_output" 2>&1; then
  printf 'expected relay update script to reject mai-server version tag\n' >&2
  exit 1
fi
assert_contains "$(< "$wrong_version_output")" "--version for mai-relay must start with mai-relay-v"

printf 'mai relay script tests passed\n'
