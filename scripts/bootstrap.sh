#!/usr/bin/env bash
set -euo pipefail

if ! command -v mise >/dev/null 2>&1; then
    curl https://mise.run | sh
    export PATH="$HOME/.local/bin:$PATH"
fi

cd "$(dirname "${BASH_SOURCE[0]}")/.."
mise trust
mise install
