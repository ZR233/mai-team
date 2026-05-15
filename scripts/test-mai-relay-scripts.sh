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

update_output="$("$ROOT_DIR/scripts/update-mai-relay-ubuntu-24.04.sh" --dry-run)"

assert_contains "$update_output" "DRY RUN: write /etc/systemd/system/mai-relay.service:"
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

printf 'mai relay script tests passed\n'
