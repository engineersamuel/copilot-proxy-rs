#!/usr/bin/env bash
set -euo pipefail

base_url="${1:-http://127.0.0.1:19090}"

curl -fsS "$base_url/health" | python -m json.tool
curl -fsS "$base_url/version" | python -m json.tool
curl -fsS "$base_url/v1/models" | python -m json.tool | head -80

curl -fsS -X POST "$base_url/v1/chat/completions" \
  -H 'Content-Type: application/json' \
  -d '{"model":"gpt-5.5","messages":[{"role":"user","content":"Hello, answer me with a funny quote"}]}' \
  | python -m json.tool

curl -fsS -X POST "$base_url/v1/messages" \
  -H 'Content-Type: application/json' \
  -d '{"model":"claude-sonnet-4-6","max_tokens":128,"messages":[{"role":"user","content":"Say hello from Rust"}]}' \
  | python -m json.tool

curl -fsS -X POST "$base_url/v1/responses" \
  -H 'Content-Type: application/json' \
  -d '{"model":"gpt-5.5","input":"Say hello from Rust Responses"}' \
  | python -m json.tool
