#!/usr/bin/env bash
set -euo pipefail

if ! command -v mise >/dev/null 2>&1; then
  if [[ ! -x "$HOME/.local/bin/mise" ]]; then
    curl -fsSL https://mise.run | sh
  fi
  export PATH="$HOME/.local/bin:$PATH"
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "warning: cargo not found; install Rust (https://rustup.rs) before running mise run proxy:up" >&2
fi

cd "$(dirname "${BASH_SOURCE[0]}")/.."
mise trust
mise install

echo "Done. In new shells, add ~/.local/bin to PATH and activate mise (e.g. eval \"\$(mise activate bash)\") so 'mise run proxy:up' works."
