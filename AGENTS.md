# rust-9router — guide for agents and contributors

Rust port of [9router](https://github.com/decolua/9router). The JS upstream is
checked out at `../9router` and is the **spec**: read the JS file before
writing its port. Never modify `../9router`.

Goal: feature parity with the JS build, with the same on-disk state and the
same HTTP surface, so existing clients and the existing database keep working:

- State dir: `$DATA_DIR`, else `~/.9router` (`nr_core::paths`). Same files as
  JS: `db/data.sqlite` (+ `db/backups/`), `jwt-secret`, `machine-id`, `mitm/`,
  `logs/`, `model-catalog*.json`, usage/log files.
- HTTP: `/v1/*`, `/v1beta/*`, `/api/*` with the same paths, JSON shapes,
  status codes and headers as the JS routes. Dashboard at `/dashboard`.
- Default port 20128.

## Build environment (this machine)

- The repo lives on an NTFS fuse mount that cannot execute binaries.
  `target/` is a symlink to `~/.cache/rust-9router-target` (ext4). Never delete
  or replace that symlink. If it is missing:
  `mkdir -p ~/.cache/rust-9router-target && ln -s ~/.cache/rust-9router-target target`.
- rustc/cargo 1.85.1 (Debian), no rustup, no wasm target. `resolver = "3"`
  picks dependency versions that build on 1.85; if a crate still needs a newer
  rustc, pin an older version range.
- 4 cores, ~3 GB free RAM, `jobs = 2`, one shared target dir. Concurrent cargo
  runs wait on each other ("Blocking waiting for file lock" is normal).
- Use `cargo check -p <crate>` while working and `cargo test -p <crate>` before
  reporting. Never run `cargo clean`. Avoid `--release` unless asked.

## Workspace map

| Crate | Ports (JS, relative to `../9router`) | Owner |
|---|---|---|
| `nr-core` | Shared contracts: `Credentials`, `Executor` trait, `http::proxy_fetch` (utils/proxyFetch.js), `Logger`, `paths` (src/lib/dataDir.js), format ids | lead (frozen) |
| `nr-data` | Embedded JSON dumps of open-sse/config, open-sse/providers, translator/schema, src/shared/constants (`tools/dump-js-data.mjs`) + Rust ports of the *functions* in those files | data |
| `nr-common` | Leaf open-sse/utils: sessionManager, sse, sseConstants, claudeCloaking, claudeSignature, reasoningContentInjector, webCookieAuth, geminiCookies, usageTracking, upstreamHeaders, clientDetector, modelMarkers, tagBlocks, debugLog, writableDataDir; services/provider.js, services/model.js, services/thoughtSignatureStore.js | data |
| `nr-db` | src/lib/db/**, localDb.js, usageDb.js, requestDetailsDb.js, disabledModelsDb.js | db |
| `nr-translator` | open-sse/translator/** (except the web-tools files), open-sse/transformer/**, utils/streamHelpers.js | translator |
| `nr-oauth` | src/lib/oauth/**, services/tokenRefresh.js + tokenRefresh/**, services/oauthCredentialManager.js, services/nous.js | oauth |
| `nr-exec-base` | executors/base.js (done), executors/default.js, shared/clineAuth.js, clineEnvelope.js, machineId.js | exec-simple |
| `nr-exec-simple` | executors: azure, vertex, gemini-cli, iflow, kimchi (+services/kimchiModels), ollama-local, xiaomi-tokenplan, xiaomi-mimo (+shared/mimoAccount), mimo-free, minimax-code (+services/minimaxCodeLease, minimaxCodeUsage), codebuddy-cn, codebuddy-intl, github (+services/copilotModels), codex (+utils/codexToolSchema), commandcode (+shared/commandCodeHeaders), opencode, opencode-go, opencode-zen (+utils/opencodeFingerprint, services/opencodeTlsClient) | exec-simple |
| `nr-exec-binary` | executors: kiro (+utils/awsEventStream, kiroSessionReplay, services/kiroModels), bedrock (+utils/awsSigv4, shared/awsCredentials), cursor (+utils/cursorProtobuf, cursorChecksum, services/cursorModels) | exec-binary |
| `nr-exec-ide` | executors: qoder (+shared/qoder/**, services/qoderModels), zed (+shared/zedAuth), trae, windsurf, antigravity | exec-ide |
| `nr-exec-cli` | executors: devin-cli, perchai, grok-cli (+services/grokTlsClient, grokCliModels) | exec-cli |
| `nr-exec-webchat` | executors: chatgpt-web (+chatgpt-web/**, chatgptWebTools), deepseek-web (+deepseek-web/**, deepseek-web-done-terminator, open-sse/lib), perplexity-web, uc-web; translator/webTools.js, deepseekWebTools.js, deepseekWebToolBinding.js | exec-webchat |
| `nr-exec-browser` | executors: gemini-web (+gemini-web/**), grok-web (+grok-web-tools), muse-web (+muse-web/**), veoaifree-web, browserExecutableCheck; services/browserPool, browserProfileCache, virtualDisplay, obscura, lazyRuntime, webIdentity, webLogin, webLoginConfig | exec-browser |
| `nr-executors` | executors/index.js | lead |
| `nr-engine` | open-sse/handlers/**, open-sse/rtk/**, the remaining open-sse/services and utils, src/sse/** | wave 2 |
| `nr-server` | src/app/api/**, the remaining src/lib, src/mitm/**, src/proxy.js, custom-server.js; dashboard UI (maud + htmx) for src/app/(dashboard) + src/shared/components | wave 2/3 |
| `nr-cli` | cli/** → the `9router` binary | cli |

Dependency direction (no cycles): core → data → common → {db, translator,
oauth} → exec-base → exec-* → executors → engine → server → cli.

## Porting rules

1. **Faithful port.** Same behavior, same edge cases. Keep JS comments that
   explain *why* (issue numbers like `#4992`, upstream quirks).
2. **Names.** JS `camelCase` function → Rust `snake_case`, module path mirrors
   the JS path (open-sse/utils/sessionManager.js → `nr_common::session_manager`).
   JSON keys stay exactly as in JS (camelCase) for DB/wire compatibility.
3. **Data, not literals.** Model lists, URLs, constants already in the nr-data
   dumps are read through `nr_data::export(...)`/helpers, never re-typed.
4. **JSON** bodies are `serde_json::Value` (insertion-ordered). Use
   `nr_core::json::{truthy, str_of, get_path}` for JS truthiness.
5. **Errors.** Public fns return `nr_core::Result`. Never panic or `unwrap` on
   data from the network, the DB, files or clients.
6. **Fail-open** wherever JS is fail-open (RTK, headroom, logging, stats).
7. **Tests.** Port the JS tests for your files from `../9router/tests/unit` and
   `../9router/tests/translator` (grep by file/feature name) as Rust `#[test]`s,
   and test every non-trivial branch you write. No live provider calls in tests;
   use a local axum mock server.
8. **Security.** Never log secrets (tokens, API keys, cookies, passwords).
   Keep redaction where JS redacts. Keep the `X-Forwarded-For` trust rules of
   custom-server.js. Keep SSRF guards on user-supplied URLs.
9. **Shortcuts.** When something can't be ported faithfully (needs a real
   browser, TLS fingerprinting, …), write the closest working version, mark it
   `// shortcut: <the limit>, <when to upgrade>`, and list it in your report.

## Coordination rules (parallel agents)

- Edit only crates you own (table above). Do not edit the root `Cargo.toml`;
  add dependencies to your own crate's `Cargo.toml` (`x.workspace = true` for
  workspace deps, or an explicit version for new crates).
- Frozen interfaces — add items, never change signatures: all of `nr-core`,
  `nr_exec_base::base`, `nr_oauth::{token_refresh, credential_manager}`,
  `nr_translator::registry::{RequestFn, ResponseFn, Registry}`,
  `nr_translator::state::StreamState` (append fields only),
  `nr_server::{ServerOptions, run}`, and each executor module's
  `pub fn create(provider: &str) -> Arc<dyn Executor>`.
- Need a function another owner hasn't ported yet? Don't edit their crate.
  Write a private helper in your crate marked
  `// TODO(dedupe): use <crate>::<path> once ported` and keep going.
- Keep your crate compiling at every step — other crates depend on it. Run
  `cargo check -p <crate>` after each file. If an error comes from a crate you
  don't own, wait and retry; don't fix it.
- No state-changing git commands (commit, stash, checkout, reset). The lead commits.
- Never write to the real `~/.9router`. Tests use temp dirs. To test against
  the real database, copy it to a temp dir first and open the copy.

## Report format (end of every agent task)

1. JS files ported → Rust modules; `cargo test -p <crate>` result.
2. Every `shortcut:` and `TODO(dedupe)` you left.
3. Functions you expect from other crates (name + JS origin).
4. Anything not ported, and why.
