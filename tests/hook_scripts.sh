#!/usr/bin/env bash
set -Eeuo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
guard="$repo_root/scripts/hooks/guard-destructive.sh"
format_check="$repo_root/scripts/hooks/check-rust-format.sh"

printf '%s\n' '{"tool_input":{"command":"cargo test --locked"}}' | "$guard"

set +e
printf '%s\n' '{"tool_input":{"command":"rm -rf /tmp/hook-test"}}' | "$guard" 2>/dev/null
guard_status=$?
set -e
if [[ "$guard_status" -ne 2 ]]; then
  printf 'expected destructive-command guard to exit 2, got %s\n' "$guard_status" >&2
  exit 1
fi

printf '%s\n' '{"tool_input":{"file_path":"README.md"}}' | "$format_check"
