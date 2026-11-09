#!/usr/bin/env bash
# End-to-end probe of a running 9router. Usage: e2e.sh <port> <outdir>
set -u
PORT="${1:-21999}"
OUT="${2:-./e2e-out}"
BASE="http://127.0.0.1:${PORT}"
mkdir -p "$OUT"

# req <name> <method> <path> [curl args...]
req() {
  local name="$1" method="$2" path="$3"; shift 3
  local body_file="$OUT/${name}.body" head_file="$OUT/${name}.head"
  local code
  code=$(curl -sS -m 20 -o "$body_file" -D "$head_file" -w '%{http_code}' \
        -X "$method" "${BASE}${path}" "$@" 2>"$OUT/${name}.err") || code="CURL_FAIL"
  {
    printf '### %s %s\n' "$method" "${BASE}${path}"
    [ "$#" -gt 0 ] && printf 'ARGS: %s\n' "$*"
    printf 'STATUS: %s\n' "$code"
    printf 'BODY: '
    head -c 2000 "$body_file" 2>/dev/null || true
    printf '\n\n'
  } >> "$OUT/results.txt"
  echo "$code"
}
