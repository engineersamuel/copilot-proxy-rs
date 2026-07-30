#!/usr/bin/env bash
set -Eeuo pipefail

payload="$(</dev/stdin)"

destructive_pattern='rm[[:space:]]+-[[:alnum:]]*r[[:alnum:]]*f|rm[[:space:]]+-[[:alnum:]]*f[[:alnum:]]*r|git[[:space:]]+reset[[:space:]]+--hard|git[[:space:]]+clean[[:space:]]+-[[:alnum:]]*f|git[[:space:]]+checkout[[:space:]]+--|git[[:space:]]+restore[^"\n]*--source'

if [[ "$payload" =~ $destructive_pattern ]]; then
  printf '%s\n' 'Blocked destructive command. Resolve exact targets and request explicit user approval.' >&2
  exit 2
fi

exit 0
