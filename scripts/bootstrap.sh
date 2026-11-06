#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

if ! command -v mise >/dev/null 2>&1; then
  if [[ -x "${HOME}/.local/bin/mise" ]]; then
    export PATH="${HOME}/.local/bin:${PATH}"
  elif command -v brew >/dev/null 2>&1; then
    brew install mise
    export PATH="$(brew --prefix mise)/bin:${PATH}"
  else
    curl --proto '=https' --tlsv1.2 -fsSL https://mise.run | sh
    export PATH="${HOME}/.local/bin:${PATH}"
  fi
fi

mise trust mise.toml
mise install
