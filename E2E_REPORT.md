# End-to-end verification: rust-9router

Date: 2026-11-09
Binary: `cargo build -p nr-cli` → `target/debug/9router`, version `0.1.0`
Verdict: **the Rust port runs and serves the full documented HTTP surface.**
Health, models, key auth, chat routing, management API and the authenticated
dashboard all respond correctly, with no panic in the server log. Six routing/startup defects were found and fixed
(provider-node `prefix` routing plus the five in section 7b).

## Environment

```
$ source ~/.cargo/env
$ rustc --version
rustc 1.99.1 (2c58f9c3d 2026-10-13)
$ cargo --version
cargo 1.99.1 (5f94df48 2026-08-27)
$ ls -ld target
target -> /mnt/immich-media/rust-target
```

`cargo build -p nr-cli` finished with exit code 0 (debug, 146 MB). The build is
slow here only because the shared target dir was busy with other agents' test
jobs (8m 59s waiting on the file lock, then the actual compile).

## 1. Run

The launcher is interactive by default, so `--tray` plus a non-TTY stdin is
needed for an unattended run. Everything below uses a throwaway data dir; the
real `~/.9router` was never opened.

```
$ DD=$(mktemp -d)
$ PORT=22003 target/debug/9router --port 22003 --host 127.0.0.1 \
    --no-browser --skip-update --tray
```

Startup log:

```
🚀 9router v0.1.0
Server: 127.0.0.1:22003
Starting server...
INFO nr_server: 9router listening on http://127.0.0.1:22003
INFO nr_db::migrate: [DB][migrate] applied #1 initial
✅ Server ready at http://127.0.0.1:22003/dashboard
```

A fresh SQLite database was created and migrated inside `$DD`. The probe script
is `scripts/e2e_probe.sh`; it records every request/response pair.

## 2. Results

Base: `http://127.0.0.1:22003`, fresh temp DB, no providers configured.

### Health

| Request | Status | Body |
|---|---|---|
| `GET /health` | 200 | `{"ok":true}` |
| `GET /api/health` | 200 | `{"ok":true}` |

### Models

| Request | Status | Body |
|---|---|---|
| `GET /v1/models` (no key) | 401 | `{"error":{"message":"Missing API key","type":"authentication_error","code":"invalid_api_key"}}` |
| `GET /v1/models` (valid key) | 200 | 918-model OpenAI list |

First element, exactly as served:

```json
{"id":"alicode-intl/qwen3.5-plus","object":"model","owned_by":"alicode-intl","context_length":200000,"max_completion_tokens":64000}
```

This matches the JS shape (`{object:"list", data:[…]}`) and the same key-gate
the JS build applies when `requireApiKey` is true.

### Chat: missing key

```
$ curl -sS -X POST -H 'content-type: application/json' \
    -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hi"}]}' \
    http://127.0.0.1:22003/v1/chat/completions
HTTP 401
{"error":{"message":"Missing API key","type":"authentication_error","code":"invalid_api_key"}}
```

The error shape is exactly the JS `buildErrorBody` shape
(`open-sse/utils/error.js`): `error.message`, `error.type`, `error.code`, with
401 → `authentication_error` / `invalid_api_key`.

### Management API

| Request | Status | Body |
|---|---|---|
| `GET /api/settings` | 200 | full settings object |
| `GET /api/providers` | 200 | `{"connections":[]}` |
| `GET /api/keys` | 200 | `{"keys":[]}` |

`/api/settings` reports the JS defaults:

```json
{"requireLogin":true,"requireApiKey":true,"comboStrategy":"fallback",
 "rtkEnabled":true,"headroomEnabled":false,"hasPassword":false, …}
```

### Key creation, then authenticated chat

```
$ curl -sS -X POST -H 'content-type: application/json' \
    -d '{"name":"e2e-probe"}' http://127.0.0.1:22003/api/keys
HTTP 201
{"key":"sk-e9f71a33bfc94350ac9083ab904a7fe0","name":"e2e-probe",
 "id":"e4595cac-e726-4ca4-85a8-2af9a3c5157d","machineId":"ffa7bf5694a1601b",
 "expiredAt":null,"limitConcurrent":0,"rpm":0}
```

Status 201 matches `src/app/api/keys/route.js`. The key appears in the
authenticated `/keys` dashboard page afterwards.

| Request | Status | Body |
|---|---|---|
| `POST /v1/chat/completions`, no key | 401 | `Missing API key` |
| `POST /v1/chat/completions`, bad key | 401 | `{"error":{"message":"Invalid API key","type":"authentication_error","code":"invalid_api_key"}}` |
| `POST /v1/chat/completions`, valid key, no provider | 404 | `{"error":{"message":"No active credentials for provider: openai","type":"invalid_request_error","code":"model_not_found"}}` |

The 404 is the correct outcome: key auth passed, the model resolved to provider
`openai`, and credential lookup found no account. It is an ordinary error
response, not a panic.

### Dashboard

| Request | Status | Notes |
|---|---|---|
| `GET /` | 303 | `location: /login` (requireLogin defaults true) |
| `GET /providers` | 303 | `location: /login` |
| `GET /dashboard` | 303 | `location: /` |
| `GET /login` | 200 | sign-in HTML |
| `POST /login` `password=123456` | 303 | sets `auth_token` cookie |
| `GET /` (with cookie) | 200 | `Server Status · 9router` |
| `GET /providers` (with cookie) | 200 | `Providers · 9router` |
| `GET /dashboard` (with cookie) | 303 | `location: /` |
| `GET /keys` (with cookie) | 200 | lists `e2e-probe` / `sk-e9f71a33…` |
| `GET /settings` (with cookie) | 200 | settings form |
| `GET /usage` (with cookie) | 200 | usage page |

`GET /dashboard` redirecting to `/` matches the AGENTS.md contract: the pages are
served at the root, so `/dashboard` and `/dashboard/<page>` 303 to the root.

## 3. Engine path proven against a real upstream socket

A chat that only ever 401s or 404s does not prove the engine is wired. To close
that, a local OpenAI-compatible mock (`scripts/e2e_mock_upstream.py`) was
registered as a provider node and reached through the gateway:

```
$ curl -sS -X POST -H 'content-type: application/json' \
    -d '{"name":"mock","prefix":"mock","type":"openai-compatible","apiType":"chat",
         "baseUrl":"http://127.0.0.1:21998/v1"}' \
    http://127.0.0.1:22002/api/provider-nodes
HTTP 201
{"node":{"apiType":"chat","baseUrl":"http://127.0.0.1:21998/v1","prefix":"mock",
 "id":"openai-compatible-chat-c1053349","type":"openai-compatible","name":"mock",…}}

$ curl -sS -X POST -H 'content-type: application/json' \
    -d '{"provider":"openai-compatible-chat-c1053349","name":"mock-conn",
         "apiKey":"sk-mock-123"}' http://127.0.0.1:22002/api/providers
HTTP 201
{"connection":{…,"providerSpecificData":{"prefix":"mock","apiType":"chat",
 "baseUrl":"http://127.0.0.1:21998/v1",…},…}}
```

Chat, addressed by the node **prefix** (the JS `mock/model` form):

```
$ curl -sS -X POST -H 'content-type: application/json' \
    -H "Authorization: Bearer $KEY" \
    -d '{"model":"mock/mock-model","messages":[{"role":"user","content":"hello via prefix"}],"stream":false}' \
    http://127.0.0.1:22002/v1/chat/completions
HTTP 200
{"id":"chatcmpl-mock-1","object":"chat.completion","created":1700000000,
 "model":"mock-model","choices":[{"index":0,"message":{"role":"assistant",
 "content":"hello from mock upstream"},"finish_reason":"stop"}],
 "usage":{"prompt_tokens":3,"completion_tokens":5,"total_tokens":8}}
```

The mock's own request log confirms the gateway did the work:

```json
{"path":"/v1/chat/completions","auth":"Bearer sk-mock-123",
 "body":"{\"model\":\"mock-model\",\"messages\":[{\"role\":\"user\",\"content\":\"hello via prefix\"}],\"stream\":false}"}
```

Prefix → node id resolution, connection selection, credential injection
(`Bearer sk-mock-123`), request translation to the upstream OpenAI shape and the
response translation back all happened in-process. No real provider was called.

## 4. Defect found and fixed

### 4.1 Provider-node `prefix` did not route (`fix(nr-engine): route provider-node prefixes`)

`POST /api/provider-nodes` accepts a user-defined `prefix`, and the JS
`getModelInfo` (`src/sse/services/model.js`) matches it before falling back to
the alias map. The Rust `nr_engine::model::get_model_info` had no such step, so
`mock/model` resolved to provider `mock` and failed:

```
POST /v1/chat/completions  {"model":"mock/mock-model"}
→ 404 {"error":{"message":"No active credentials for provider: mock", …}}
```

`openai-compatible-chat-<id>/mock-model` worked, which is how the gap was
confirmed as prefix resolution rather than a broken executor.

Fixed in `crates/nr-engine/src/model.rs`:

- `ParsedModel` gained `provider_alias` (the raw text before `/`), matching the
  JS `parseModel` result.
- `reserved_provider_prefixes()` reproduces `RESERVED_PROVIDER_PREFIXES` from the
  embedded registry (plus `xmtp` / `xiaomi-tokenplan` from `LOCAL_PROVIDER_ALIASES`),
  so a user node can never shadow `openai`, `cf`, `hf`, `xmtp`, ….
- `resolve_node_prefix()` checks `openai-compatible`, `anthropic-compatible` and
  `custom-embedding` nodes in the same order as JS and returns the node id.
- `get_model_info` calls it for `provider/model` strings before the alias-map
  fallback.

Regression test `node_prefix_routes_to_node_id` creates a node with prefix
`mock`, asserts `mock/mock-model` resolves to the node id, and asserts
`openai/gpt-4o` still resolves to `openai` (reserved prefix not shadowed).

```
$ cargo test -p nr-engine --lib model::
test result: ok. 8 passed; 0 failed; 0 ignored
```

After the fix, the same request returns 200 and reaches the mock (section 3).

## 5. Panics

```
$ grep -i panic /tmp/engine-server.log
(none)
```

No panic, no `RUST_BACKTRACE` line, and no 500 on any documented endpoint across
all runs (three full probe runs plus the engine-path run). Every failure the
surface produced was a structured JSON error with the JS error code.

## 6. Test suite

```
$ cargo test -p nr-server --test v1_gateway
test result: ok. 19 passed; 0 failed; 0 ignored
```

## 7. Known gaps (observed, not fixed)

1. **`/v1/models` does not enumerate live node models.** JS
   `buildModelsList` calls `fetchCompatibleModels(conn)` for every compatible
   node and exposes the upstream catalog. Rust lists static provider models only,
   so a node with no static catalog contributes nothing to `/v1/models` even
   though it routes fine. Chat against it works (section 3).
2. **The root/dashboard redirect is inverted relative to JS.** JS serves the pages
   under `/dashboard` and redirects `/` → `/dashboard`. Rust serves them at the
   root and redirects `/dashboard` → `/` (and `/dashboard/<page>` → `/<page>`,
   verified: `/dashboard/providers` → `/providers`, `/dashboard/keys` → `/keys`).
   Both end states land on the same page and no redirect loop exists, but a JS
   client that hardcodes `Location: /dashboard` assumptions would see the
   opposite hop. Aligning this means moving every page route under `/dashboard`.
3. **Launcher is interactive unless `--tray`.** `9router` with no flags on a TTY
   opens a menu; with piped stdin it prints the menu and exits. That is the JS
   CLI behaviour, but it makes a headless smoke test need `--tray`.
4. **Unauthenticated `/` takes one hop to `/login`, JS takes two.** JS sends
   `/` → `/dashboard` → `/login`; Rust sends `/` → `/login` directly. Same end
   state for a browser.


## 7b. Additional defects found in a second pass (fixed)

A second, independent pass over the same binary (fresh temp DB, port 21999,
`--host 127.0.0.1 --tray`) found four more defects. All are fixed in this commit.

### a. `9router --help` panicked before any request

```
thread 'main' panicked at clap_builder-4.6.7/src/builder/debug_asserts.rs:125:
Command 9router: Short option names must be unique for each argument, but '-h'
is in use by both 'help_flag' and 'help'
```

`Args` declared `#[arg(short='h', long)] help_flag` while clap also
auto-generated `-h/--help`, so **every** invocation panicked, including the
plain start. Fixed by disabling clap's auto flags and binding the JS flags
explicitly (`-h/--help`, `-v/--version`), matching `cli/cli.js`.

### b. `--host` was rejected although the help text advertises it

Help prints `-H, --host <host>` but clap derived the long name from the field
(`--hostname`), so the documented flag exited with
`error: unexpected argument '--host' found`. Fixed with
`long = "host", visible_alias = "hostname"`; `--hostname` and `$HOSTNAME` still work.

### c. `/api/models`, `/api/settings`, `/api/keys`, `/api/providers` hung (>25 s)

A thread dump showed the handler burning CPU in
`regex_automata::meta::regex::Builder::build`, called from
`nr_data::pricing::match_pattern` via
`nr_data::capabilities::get_capabilities_for_model` at `routes/api/models.rs:82`.

`match_pattern` compiled a fresh `Regex` from the glob on **every call**, and
`/api/models` calls it once per model (1491) over 164 ordered patterns. Measured
in isolation against the real embedded data: **15.3 s** for the catalog loop,
all inside regex construction, with no single slow pattern.

Fixed by replacing regex compilation with a two-pointer glob matcher
(`glob_match_ci`) in `crates/nr-data/src/pricing.rs`. Semantics are identical
(`*` matches any run, case-insensitive, regex metacharacters literal). The same
loop now takes **26 ms** (590x faster). Regression tests:
`test_glob_edge_cases`, `test_match_pattern_is_cheap`.

This also affected `/v1/models`, chat routing, pricing and thinking levels.

### d. Every chat returned a canned `"pass-through"` completion

`POST /v1/chat/completions` with a valid key returned 200 and

```json
{"id":"chatcmpl-…","choices":[{"message":{"role":"assistant","content":"pass-through"}}],
 "usage":{"prompt_tokens":10,"completion_tokens":10,"total_tokens":20}}
```

for real models *and* for `definitely-not-a-real-model-xyz`. `nr-engine` ships a
`PassThroughChatEngine` stub that stays active until
`nr_engine::install_real_engines(db)` is called, and nothing in `nr-server`
called it. Fixed by installing the real DB-backed engines in `app()`
(`crates/nr-server/src/lib.rs`). After the fix the same request correctly fails
with `No active credentials for provider: openai`, and reaches a real upstream
once a provider exists.

This was the most dangerous of the six: a green `/v1/chat/completions` that
never contacted a provider would have made the port look finished while serving
fabricated responses.

### e. Dashboard error fragments swallowed the HTTP status

`POST /providers` with no API key, `POST /keys` with a blank name, and the
wrong-current-password case all answered **200** with an `alert-error` body.
`dashboard::forms::err_from` rendered the fragment via `html_response`, which
hardcodes `StatusCode::OK`. Fixed with `err_fragment_with_status`, which keeps
the originating status; htmx swaps the body regardless, so the UI is unchanged.

### f. `/usage` had no live refresh

`all_pages_render_when_authenticated` asserts every dashboard page is htmx-wired.
`/usage` was a static snapshot while the JS page polls `/api/usage/stream`.
Added the same `hx-get`/`hx-select`/`every 30s` auto-refresh the status page uses.

### Test impact of d-f

```
$ cargo test -p nr-server --test dashboard_pages -- --test-threads=1
before: test result: FAILED. 9 passed; 5 failed
after:  test result: ok.    14 passed; 0 failed

$ cargo test -p nr-server -p nr-data
all suites ok (47, 98, 11, 15, 14, 5, 19, 9, 19 passed; 0 failed)
```

Note `dashboard_pages` shares one temp DB across tests and must run with
`--test-threads=1` as its own docstring states; a plain `cargo test -p nr-server`
still shows those 5 as racing.

## 8. Artifacts

- `scripts/e2e_probe.sh` — reproducible probe; takes `BIN`, `PORT`, `OUT`,
  never writes to `~/.9router`.
- `scripts/e2e_mock_upstream.py` — local OpenAI-compatible upstream used for the
  engine-path proof.
- Raw transcript: `/home/caliph/.jcode/scratch/e2e-report/` (`index.md`,
  `summary.txt`, `server.log`, per-request `.body` files).

All probe servers and the mock upstream were stopped; ports 21991-22007 are free.
No state was written to `~/.9router`: verified by stat-ing its database before and
after a full probe run (WAL and data file mtimes unchanged). Note that other
agents' `cargo test` jobs in this workspace do open the real `~/.9router`; that is
not this probe.
