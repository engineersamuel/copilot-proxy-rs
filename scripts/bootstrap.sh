#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
if ! command -v mise >/dev/null 2>&1; then
    if [[ ! -x "$HOME/.local/bin/mise" ]]; then
        curl -fsSL https://mise.run | sh
    fi
    export PATH="$HOME/.local/bin:$PATH"
fi

mise trust
mise install
