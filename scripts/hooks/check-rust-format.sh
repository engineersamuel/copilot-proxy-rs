#!/usr/bin/env bash
set -Eeuo pipefail

payload="$(</dev/stdin)"
rust_path_pattern='"file_path"[[:space:]]*:[[:space:]]*"[^"]*\.rs"'

if [[ ! "$payload" =~ $rust_path_pattern ]]; then
  exit 0
fi

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)"
cd -- "$repo_root"
cargo fmt --check
