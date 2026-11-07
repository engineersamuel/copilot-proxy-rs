#!/usr/bin/env bash
set -euo pipefail

if ! command -v mise >/dev/null 2>&1; then
  if [[ ! -x "$HOME/.local/bin/mise" ]]; then
    if ! command -v curl >/dev/null 2>&1; then
      echo "curl is required to install mise; install curl or mise manually" >&2
      exit 1
    fi
    curl -fsSL https://mise.run | MISE_VERSION="${MISE_VERSION:-v2026.10.24}" sh
  fi
  export PATH="$HOME/.local/bin:$PATH"
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "warning: cargo not found; install Rust (https://rustup.rs) then re-run scripts/bootstrap.sh; mise run proxy:up will fail without it" >&2
fi

cd "$(dirname "${BASH_SOURCE[0]}")/.."
mise trust
mise install

echo "Done. In new shells, add ~/.local/bin to PATH and activate mise (e.g. eval \"\$(mise activate bash)\") so 'mise run proxy:up' works."
