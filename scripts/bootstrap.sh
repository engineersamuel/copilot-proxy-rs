#!/usr/bin/env bash
# Install mise if missing, then install the pinned tools from mise.toml.
set -euo pipefail

if ! command -v mise >/dev/null 2>&1; then
  curl -fsSL https://mise.run | sh
  export PATH="$HOME/.local/bin:$PATH"
fi

cd "$(dirname -- "${BASH_SOURCE[0]}")/.."
mise install
