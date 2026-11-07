#!/usr/bin/env bash
# Install mise when missing, then install the pinned tools.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

if ! command -v mise >/dev/null 2>&1; then
  if [[ -x "$HOME/.local/bin/mise" ]]; then
    export PATH="$HOME/.local/bin:$PATH"
  else
    curl -fsSL https://mise.run | sh
    export PATH="$HOME/.local/bin:$PATH"
  fi
fi

mise trust --yes mise.toml
mise install
