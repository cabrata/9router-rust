#!/usr/bin/env bash
# Full end-to-end probe for a running 9router. Usage: e2e_run.sh <port> <outdir>
set -u
PORT="${1:-21999}"
OUT="${2:-./e2e-out}"
BASE="http://127.0.0.1:${PORT}"
rm -rf "$OUT"; mkdir -p "$OUT"
: > "$OUT/results.txt"

# req <name> <method> <path> [curl args...]
req() {
  local name="$1" method="$2" path="$3"; shift 3
  local code
  code=$(curl -sS -m 25 -o "$OUT/${name}.body" -D "$OUT/${name}.head" -w '%{http_code}' \
        -X "$method" "${BASE}${path}" "$@" 2>"$OUT/${name}.err") || code="CURL_FAIL"
  {
    printf '### %s %s\n' "$method" "${BASE}${path}"
    [ "$#" -gt 0 ] && printf 'ARGS: %s\n' "$*"
    printf 'STATUS: %s\n' "$code"
    printf 'RESP: %s\n\n' "$(head -c 3000 "$OUT/${name}.body" 2>/dev/null)"
  } >> "$OUT/results.txt"
  printf '%-28s %s\n' "$name" "$code"
  LAST_CODE="$code"
}

echo "== read-only endpoints =="
req health                 GET /health
req api_health             GET /api/health
req v1_models              GET /v1/models
req api_models             GET /api/models
req api_settings           GET /api/settings
req api_keys               GET /api/keys
req api_providers          GET /api/providers
req api_combos             GET /api/combos
req api_tags               GET /api/tags
req api_version            GET /api/version
req api_usage_stats        GET /api/usage/stats
req api_init               GET /api/init
req api_models_alias       GET /api/models/alias
req dashboard_root         GET /dashboard
req dashboard_page         GET /dashboard/keys

echo "== auth: chat without a key =="
req chat_nokey POST /v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hi"}]}'

req messages_nokey POST /v1/messages \
  -H 'content-type: application/json' \
  -d '{"model":"claude-3-5-sonnet-20241022","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}'

echo "== create key then retry =="
req key_create POST /api/keys -H 'content-type: application/json' -d '{"name":"e2e-probe"}'
KEY=$(sed -n 's/.*"key" *: *"\([^"]*\)".*/\1/p' "$OUT/key_create.body" | head -1)
echo "KEY=${KEY:-<none>}"

if [ -n "${KEY:-}" ]; then
  req chat_withkey POST /v1/chat/completions \
    -H 'content-type: application/json' -H "Authorization: Bearer $KEY" \
    -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hi"}]}'
  req messages_withkey POST /v1/messages \
    -H 'content-type: application/json' -H "Authorization: Bearer $KEY" \
    -d '{"model":"claude-3-5-sonnet-20241022","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}'
  req badmodel_withkey POST /v1/chat/completions \
    -H 'content-type: application/json' -H "Authorization: Bearer $KEY" \
    -d '{"model":"definitely-not-a-real-model-xyz","messages":[{"role":"user","content":"hi"}]}'
  req chat_badkey POST /v1/chat/completions \
    -H 'content-type: application/json' -H "Authorization: Bearer sk-bogus" \
    -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hi"}]}'
fi

echo "done -> $OUT/results.txt"
