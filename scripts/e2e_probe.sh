#!/usr/bin/env bash
# End-to-end probe for the 9router binary. Writes raw request/response pairs to
# $OUT (default: $JCODE_SCRATCH_DIR/e2e). Never touches ~/.9router.
set -u

BIN="${BIN:-/media/caliph/project/rust-9router/target/debug/9router}"
OUT="${OUT:-${JCODE_SCRATCH_DIR:-/tmp}/nr-e2e-$(date +%s)}"
PORT="${PORT:-21991}"
BASE="http://127.0.0.1:${PORT}"
DD=$(mktemp -d "${TMPDIR:-/tmp}/nr-e2e-data.XXXXXX")
mkdir -p "$OUT"
LOG="$OUT/server.log"

echo "OUT=$OUT"
echo "DATA_DIR=$DD"
echo "PORT=$PORT"

# --- record helper: prints the exact curl command + status + body -------------
req() {
  local name="$1"; shift
  local path="$1"; shift
  local out="$OUT/$name"
  local code
  # Quote each argument so the recorded command is copy-pasteable.
  local cmd=""
  local a
  for a in "$@"; do cmd+="$(printf '%q' "$a") "; done
  code=$(curl -sS --max-time 30 -o "$out.body" -w '%{http_code}' "$@" "${BASE}${path}")
  echo "--- $name | $path -> $code" >> "$OUT/summary.txt"
  {
    echo "### $name"
    echo '```'
    echo "\$ curl -sS ${cmd}${BASE}${path}"
    echo "HTTP $code"
    head -c 2000 "$out.body"
    echo
    echo '```'
  } >> "$OUT/index.md"
  echo "[$name] $code"
}

: > "$OUT/summary.txt"
: > "$OUT/index.md"
echo "# raw e2e transcript" >> "$OUT/index.md"

# --- start server -----------------------------------------------------------
# `exec` so $! is the server itself, not a wrapping subshell (a plain subshell
# would survive the kill and keep the port).
(
  cd "$(dirname "$BIN")" || exit 1
  exec env DATA_DIR="$DD" PORT="$PORT" "$BIN" \
    --port "$PORT" --host 127.0.0.1 --no-browser --skip-update --tray \
    >"$LOG" 2>&1
) &
LAUNCH_PID=$!

for _ in $(seq 1 100); do
  if curl -sS --max-time 1 "$BASE/health" >/dev/null 2>&1; then break; fi
  sleep 0.2
done

echo "=== health ==="
req 01-health /health
req 02-api-health /api/health
echo "=== models ==="
req 03-v1-models-nokey /v1/models
echo "=== chat no key ==="
req 04-chat-nokey /v1/chat/completions -X POST -H 'content-type: application/json' \
  -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hi"}]}'
echo "=== management ==="
req 05-api-settings /api/settings
req 06-api-providers /api/providers
req 07-api-keys /api/keys

echo "=== create key ==="
KEYJSON=$(curl -sS --max-time 30 -X POST -H 'content-type: application/json' \
  -d '{"name":"e2e-probe"}' "$BASE/api/keys")
echo "$KEYJSON" > "$OUT/08-create-key.body"
{
  echo "### 08-create-key"
  echo '```'
  echo "\$ curl -sS -D- -X POST -H 'content-type: application/json' -d '{\"name\":\"e2e-probe\"}' $BASE/api/keys"
  echo "HTTP 200/201"
  echo "$KEYJSON"
  echo '```'
} >> "$OUT/index.md"
echo "[08-create-key] $KEYJSON" | head -c 400; echo
KEY=$(printf '%s' "$KEYJSON" | sed -n 's/.*"key"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
echo "KEY=${KEY:0:12}…"

req 09-v1-models-withkey /v1/models -H "Authorization: Bearer $KEY"
req 10-chat-withkey /v1/chat/completions -X POST -H 'content-type: application/json' \
  -H "Authorization: Bearer $KEY" \
  -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hi"}],"stream":false}'
req 11-chat-badkey /v1/chat/completions -X POST -H 'content-type: application/json' \
  -H "Authorization: Bearer sk-not-a-real-key" \
  -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hi"}]}'

echo "=== dashboard (unauthenticated) ==="
req 12-root / -D "$OUT/12-root.headers"
req 13-providers /providers -D "$OUT/13-providers.headers"
req 14-dashboard /dashboard -D "$OUT/14-dashboard.headers"
req 15-login /login

# requireLogin defaults to true, so the pages above redirect to /login. Sign in
# with the documented default password and fetch them again.
JAR="$OUT/cookies.txt"
rm -f "$JAR"
LOGIN_CODE=$(curl -sS --max-time 30 -o /dev/null -w '%{http_code}' -c "$JAR" \
  -X POST -d 'password=123456' "$BASE/login")
echo "[16-login-submit] $LOGIN_CODE"
echo "--- 16-login-submit | POST /login -> $LOGIN_CODE" >> "$OUT/summary.txt"
req 17-root-auth / -b "$JAR"
req 18-providers-auth /providers -b "$JAR"
req 19-dashboard-auth /dashboard -b "$JAR" -D "$OUT/19-dashboard.headers"
req 20-keys-auth /keys -b "$JAR"
req 21-settings-auth /settings -b "$JAR"
req 22-usage-auth /usage -b "$JAR"

echo "=== stop ==="
kill -TERM "$LAUNCH_PID" 2>/dev/null
# launcher spawns the listener in-process; also stop via pidfile
for _ in $(seq 1 50); do
  kill -0 "$LAUNCH_PID" 2>/dev/null || break
  sleep 0.2
done
kill -KILL "$LAUNCH_PID" 2>/dev/null
# Anchored on the binary path so this never matches the calling shell.
pkill -f "^${BIN} --port ${PORT} " 2>/dev/null
sleep 0.5
echo "server log: $LOG"
echo "=== panics in log ==="
grep -n -i "panic\|RUST_BACKTRACE" "$LOG" || echo "(none)"
echo "=== log tail ==="
tail -25 "$LOG"
echo "SUMMARY=$OUT/summary.txt"
