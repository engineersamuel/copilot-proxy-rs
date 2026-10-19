#!/usr/bin/env bash
# Repeatability check for POST /v1/models (bash 3.2 safe).
# Usage: scripts/router-repeat.sh BASE_URL PROMPTS_FILE N [AUTH_BEARER_ENV_NAME]
# Per prompt: pick distribution and agreement. Errors (non-200 / non-JSON) are
# excluded from agreement and make the exit code non-zero. No keys are printed;
# the auth header and prompt body go to curl via stdin config, never argv.
set -u
base=${1:?base url}; file=${2:?prompts file}; runs=${3:?runs}
authvar=${4:-}
token=""
if [ -n "$authvar" ]; then token=${!authvar:-}; fi
total=0; agree=0; errors=0; prompts=0
while IFS= read -r prompt || [ -n "$prompt" ]; do
  [ -z "$prompt" ] && continue
  prompts=$((prompts + 1))
  picks=""
  i=0
  while [ "$i" -lt "$runs" ]; do
    i=$((i + 1))
    body=$(jq -cn --arg i "$prompt" '{input:$i}')
    cfg=$(printf 'url = "%s/v1/models"\nrequest = "POST"\nheader = "content-type: application/json"\n' "$base")
    if [ -n "$token" ]; then cfg="$cfg"$'\n'"header = \"Authorization: Bearer $token\""; fi
    cfg="$cfg"$'\n'"data = @-"
    # Config comes via -K on fd 3; body via stdin: nothing sensitive in argv.
    out=$(printf '%s' "$body" | curl -s -w '\n%{http_code}' -K <(printf '%s\n' "$cfg") 2>/dev/null) || out=$'\n000'
    code=${out##*$'\n'}; json=${out%$'\n'*}
    pick=$(printf '%s' "$json" | jq -r '.recommended.id // empty' 2>/dev/null || true)
    if [ "$code" = "200" ] && [ -n "$pick" ]; then picks="$picks$pick"$'\n'; else picks="${picks}ERROR"$'\n'; errors=$((errors + 1)); fi
  done
  ok=$(printf '%s' "$picks" | grep -v '^ERROR$' || true)
  dist=$(printf '%s' "$picks" | sort | uniq -c | sort -rn)
  if [ -n "$ok" ]; then
    top=$(printf '%s\n' "$ok" | sort | uniq -c | sort -rn | head -1 | awk '{print $1}')
    n=$(printf '%s\n' "$ok" | wc -l | tr -d ' ')
  else top=0; n=0; fi
  printf '%s\n%s\n  agreement: %s/%s\n\n' "${prompt:0:70}" "$dist" "$top" "$n"
  total=$((total + n)); agree=$((agree + top))
done <"$file"
if [ "$prompts" -eq 0 ]; then echo "no prompts" >&2; exit 2; fi
echo "overall agreement: $agree/$total, errors: $errors"
[ "$errors" -eq 0 ]
