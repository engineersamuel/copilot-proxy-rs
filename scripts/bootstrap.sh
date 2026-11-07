#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

if ! command -v mise >/dev/null 2>&1; then
  export PATH="${HOME}/.local/bin:${PATH}"
  if ! command -v mise >/dev/null 2>&1; then
    curl -fsSL https://mise.run | MISE_INSTALL_PATH="${HOME}/.local/bin/mise" sh
  fi
fi

mise install
