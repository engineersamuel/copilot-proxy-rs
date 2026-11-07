#!/usr/bin/env bash
# Install mise when missing, then install the pinned tools.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo not found; install Rust with rustup (https://rustup.rs) first" >&2
  exit 1
fi

path_hint=false
if ! command -v mise >/dev/null 2>&1; then
  if [[ ! -x "$HOME/.local/bin/mise" ]]; then
    curl -fsSL https://mise.run | sh
  fi
  export PATH="$HOME/.local/bin:$PATH"
  path_hint=true
fi

mise trust --yes mise.toml
mise install

if [[ "$path_hint" == true ]]; then
  echo "note: mise is in ~/.local/bin, which is not on your PATH."
  echo "      Add it before you run 'mise run proxy:up', for example:"
  echo "      export PATH=\"\$HOME/.local/bin:\$PATH\""
fi
