# Port gap report: `9router` (JS) → `rust-9router`

Evidence base, produced 2026-11-09 by mechanical diff, not by reading crate READMEs:

- **Routes**: `find ../9router/src/app/api -name route.js` → 180 route files, normalised
  `[id]` → `{id}` / `[...x]` → `{*x}` and matched against every `"/api/..."` / `"/v1..."` /
  `"/v1beta..."` literal in `crates/**/*.rs`.
- **Services**: `ls ../9router/open-sse/services/*.js` → 34 modules; each module's exported
  symbol names grepped against `crates/`.
- **lib**: `find ../9router/src/lib -name '*.js'` → 129 modules; module basename and key
  exported symbols grepped against `crates/`.
- **CLI**: `../9router/cli/src/cli/commands/` (4 command modules) vs `crates/nr-cli/src/`.

Totals: **127 / 180 routes present** (70.6%), **53 missing**. Services and lib are summarised
per module below.

## COVERED

Routes with a registered Rust handler.

| JS path | Rust location |
|---|---|
| `auth/login` `auth/logout` `auth/status` `auth/reset-password` | `nr-server/src/routes/auth.rs` |
| `auth/oidc/start` `auth/oidc/callback` `auth/oidc/test` | `nr-server/src/routes/auth.rs` |
| `auth/saml/start` `auth/saml/acs` `auth/saml/metadata` `auth/saml/test` | `nr-server/src/routes/auth.rs` |
| `cli-tools/all-statuses` | `nr-server/src/routes/cli_tools.rs` |
| `cli-tools/{claude,cline,codewhale,codex,copilot,cowork,crush,deepseek-tui,devin,droid,forge,grok-build,hermes,jcode,kilo,omp,openclaw,opencode,pi,smelt}-settings` | `nr-server/src/routes/cli_tools.rs` |
| `cli-tools/codex-profiles` `cli-tools/hermes-profiles` | `nr-server/src/routes/cli_tools.rs` |
| `cli-tools/cowork-mcp-registry` `cli-tools/cowork-mcp-tools` | `nr-server/src/routes/cli_tools.rs` |
| `cli-tools/antigravity-mitm` `cli-tools/antigravity-mitm/alias` | `nr-server/src/routes/cli_tools.rs` |
| `combos` `combos/[id]` `combos/presets` | `nr-server/src/routes/api/combos.rs` |
| `health` `init` `locale` `shutdown` `tags` | `nr-server/src/routes/api/{health,init,locale,shutdown,tags}.rs` |
| `keys` `keys/[id]` | `nr-server/src/routes/api/keys.rs` |
| `media-providers/tts/voices` + `elevenlabs` `deepgram` `inworld` `minimax` | `nr-server/src/routes/api/media_providers.rs` |
| `models` `models/alias` `models/availability` `models/catalog-sync` `models/custom` `models/disabled` `models/test` | `nr-server/src/routes/api/models.rs` |
| `oauth/[provider]/[action]` | `nr-server/src/routes/oauth_flow.rs` |
| `pricing` | `nr-server/src/routes/api/pricing.rs` |
| `provider-nodes` `provider-nodes/[id]` `provider-nodes/validate` | `nr-server/src/routes/api/provider_nodes.rs` |
| `providers` `providers/client` `providers/[id]` `providers/[id]/models` `providers/[id]/overrides` `providers/[id]/test` `providers/[id]/test-models` | `nr-server/src/routes/api/providers.rs` |
| `providers/kilo/free-models` `providers/suggested-models` `providers/test-batch` `providers/validate` `providers/web-login` | `nr-server/src/routes/api/providers.rs` |
| `proxy-pools` `proxy-pools/[id]` `proxy-pools/[id]/test` | `nr-server/src/routes/api/proxy_pools.rs` |
| `proxy-pools/{cloudflare,deno,netlify,vercel}-deploy` | `nr-server/src/routes/api/proxy_pools.rs` |
| `settings` `settings/database` `settings/proxy-test` `settings/require-login` | `nr-server/src/routes/api/settings.rs` |
| `usage/{chart,history,logs,providers,request-details,request-log-entries,request-logs,stats,stream}` | `nr-server/src/routes/usage.rs` |
| `usage/[connectionId]` `…/claude-reset` `…/codex-reset-credits` | `nr-server/src/routes/usage.rs` |
| `version` `version/shutdown` `version/update` | `nr-server/src/routes/api/version.rs` |
| `v1` `v1/models` `v1/models/info` `v1/models/[...model]` | `nr-server/src/v1/models.rs` |
| `v1/chat/completions` `v1/messages` `v1/messages/count_tokens` `v1/responses` `v1/responses/compact` `v1/api/chat` | `nr-server/src/v1/chat.rs` |
| `v1/{embeddings,images/generations,audio/speech,audio/transcriptions,audio/voices,search,web/fetch,systemone}` | `nr-server/src/v1/media.rs` |
| `v1/videos/{generations,edits,extensions,[id]}` | `nr-server/src/v1/media.rs` |
| `v1beta/models` `v1beta/models/[...path]` | `nr-server/src/v1/v1beta.rs` |

Support modules confirmed present with real logic (not stubs):

| JS module | Rust location |
|---|---|
| `open-sse/services/accountFallback.js` | `nr-engine/src/account_fallback.rs` |
| `open-sse/services/capacityAdapter.js` | `nr-common/src/capacity_adapter.rs` |
| `open-sse/services/systemoneRouter.js` | `nr-common/src/systemone_router.rs` |
| `open-sse/services/thoughtSignatureStore.js` | `nr-common/src/thought_signature_store.rs` |
| `open-sse/services/tokenRefresh.js` + `tokenRefresh/**` | `nr-oauth/src/token_refresh.rs` |
| `open-sse/services/oauthCredentialManager.js` | `nr-oauth/src/credential_manager.rs` |
| `open-sse/services/{browserPool,browserProfileCache,lazyRuntime,obscura,virtualDisplay,webIdentity,webLogin,webLoginConfig}.js` | `nr-exec-browser/src/services/*` |
| `open-sse/services/{copilotModels,cursorModels,grokCliModels,kiroModels,qoderModels,clinepassModels}.js` | `nr-exec-{simple,binary,ide,cli}` model modules |
| `open-sse/services/{provider,model}.js` | `nr-common/src/{provider,model}.rs` |
| `open-sse/providers/{capabilities,catalogOverride,pricing,visionPatterns,thinkingLevels}` | `nr-data/src/*` |
| `src/lib/db/**` (repos, migrations, adapters) | `nr-db/src/**` |
| `src/lib/oauth/providers/*.js` (30 providers) | `nr-oauth/src/flows/providers/*` |
| `src/lib/qoder/*` | `nr-exec-ide/src/qoder/*` |
| `src/mitm/**` | `nr-server/src/mitm/**` |
| `src/lib/auth/{oidc,saml}.js` | inline in `nr-server/src/routes/auth.rs` |
| `cli/src/cli/commands/{connect,connectTools,show}.js` | `nr-cli/src/{cli_commands,connect_tools}.rs` |

## PARTIAL

Present but with a known behavioural gap.

| JS path / module | Rust location | Gap |
|---|---|---|
| `open-sse/services/emptyFailover.js` | *absent* | `deltaHasClientContent` / `inspectSSELine` / `scanSSEBuffer` / `peekStreamForContent` / `reconstructStream` have no Rust analogue; empty-response failover cannot detect a zero-content stream. |
| `open-sse/services/combo.js` | `nr-engine/src/combo/mod.rs` | `reorderByCapabilities`, `detectRequiredCapabilities`, `getRotatedModels`, `resetComboRotation`, `getComboModelsFromData` not found by name. |
| `open-sse/services/kimchiModels.js` | `nr-exec-simple/src/kimchi.rs` (executor only) | Model catalog discovery (`buildKimchiModelsUrl`, `resolveKimchiModels`, `getCachedKimchiModelMetadata`, `clearKimchiCatalog`) missing. |
| `open-sse/services/minimaxCodeUsage.js` | `nr-exec-simple/src/minimax_code.rs` (lease only) | `signedAccountRequest`, `getMiniMaxCodeIdentity`, `parseMembership`, `parsePlanWindows`, `getMiniMaxCodeUsage` missing (usage/plan surface). |
| `open-sse/services/opencodeTlsClient.js` | *absent* | TLS-fingerprint client for opencode not ported; executor uses plain `reqwest`. |
| `open-sse/services/projectId.js` | *absent* | Antigravity/project-id cache (fetch/invalidate/TTL cleanup) not ported. |
| `open-sse/services/usage.js` | *absent* | `getUsageForProvider` per-provider usage probe. |
| `open-sse/services/compact.js` | *absent* | `getComboModelsFromData` / `handleComboChat` compaction variant. |
| `src/lib/consoleLogBuffer.js` | *absent* | In-process log ring buffer + SSE fan-out. |
| `src/lib/headroom/{detect,process}.js` | *absent* | Detection, managed proxy spawn/stop, extras install. |
| `src/lib/pxpipe/{service,loader,install,events}.js` | *absent* | Library-mode token-saver, install log, event stats. |
| `src/lib/tunnel/**` (16 files, 1836 lines) | *absent* | Cloudflare quick tunnel + Tailscale funnel managers, health/dns probes, state. |
| `src/lib/mcp/stdioSseBridge.js` | *absent* | stdio↔SSE MCP bridge, smart text filter. |
| `src/lib/appUpdater.js`, `src/lib/updater/updater.js` | `nr-server/src/routes/api/version.rs` | Version route exists; updater download/verify/restart flow does not. |
| `src/lib/auth/{dashboardSession,loginLimiter,trustedPeer}.js` | *absent* | Session cookie helper, login rate limiter, trusted-proxy peer check. |
| `src/lib/modelCatalog/sync.js` | `nr-server/src/routes/api/models.rs` (route only) | Catalog refresh/merge engine not ported. |
| `src/lib/network/{connectionProxy,outboundProxy,netlifyRelay,proxyTest}.js` | `nr-core/src/http.rs`, `settings.rs` (partial) | `connectionProxy`/`outboundProxy` echoed in settings, but outbound-proxy resolution and Netlify relay are not implemented. |
| `src/lib/providerNormalization.js`, `src/lib/providerConnectionResponse.js` | `nr-server/src/routes/api/providers.rs` | `to_provider_connection_response` covers redaction; normalisation helpers not separately ported. |
| `src/lib/comboPresets.js` | `nr-server/src/routes/api/combos.rs` | Presets endpoint present; preset table provenance unclear (verify against JS on changes). |
| `src/lib/mimoLoginSession.js`, `src/lib/oauth/perchPlan.js`, `src/lib/oauth/kiroExternalIdp.js` | `nr-oauth` (partial) | Provider flows exist; the session/plan helper surfaces behind the OAuth sub-routes do not. |
| `cli/src/cli/commands/xaiVideo.js` | `nr-cli/src/cli_commands.rs:761` | Argument parsing ported; request/poll/download marked "unchanged stub". |
| `cli/src/cli/menus/*.js` (5 menus), `cli/src/cli/tray/*` | `nr-cli/src/main.rs` | Interactive menus and tray are not ported (`tray` feature is opt-in scaffolding). |

## MISSING

### API routes (53)

| JS path | Rust location |
|---|---|
| `headroom/status` `headroom/start` `headroom/stop` `headroom/restart` `headroom/extras` `headroom/proxy/[...path]` | — |
| `pxpipe/status` `pxpipe/health` `pxpipe/stats` `pxpipe/logs` `pxpipe/start` `pxpipe/stop` `pxpipe/restart` `pxpipe/install` | — |
| `mcp/[plugin]/sse` `mcp/[plugin]/message` | — |
| `translator/console-logs` `translator/console-logs/stream` | — |
| `translator/load` `translator/save` | — |
| `translator/translate` `translator/send` | — |
| `tunnel/status` `tunnel/enable` `tunnel/disable` | — |
| `tunnel/tailscale-check` `tunnel/tailscale-enable` `tunnel/tailscale-disable` `tunnel/tailscale-install` | — |
| `oauth/codex/bulk-import` `oauth/codex/import-token` | — |
| `oauth/cursor/auto-import` `oauth/cursor/import` | — |
| `oauth/gitlab/pat` | — |
| `oauth/grok-cli/bulk-import` | — |
| `oauth/commandcode/auto-import` | — |
| `oauth/iflow/cookie` | — |
| `oauth/kiro/api-key` `oauth/kiro/auto-import` `oauth/kiro/import` `oauth/kiro/import-cli-proxy` `oauth/kiro/social-authorize` `oauth/kiro/social-exchange` | — |
| `oauth/perchai/auto-import` `oauth/perchai/import` `oauth/perchai/plan/claim` `oauth/perchai/plan/status` | — |
| `oauth/xiaomi-mimo/api-key` `oauth/xiaomi-mimo/auto-import` `oauth/xiaomi-mimo/login/start` `oauth/xiaomi-mimo/login/status` | — |
| `oauth/zed/auto-import` `oauth/zed/import` | — |

The 32 `oauth/*` sub-routes all hang off providers whose Rust flows already exist in
`nr-oauth/src/flows/providers/` — they are credential **import** surfaces (paste a token,
read an IDE's local store) plus a few interactive login sessions, not new providers.

### Services and lib modules

`open-sse/services/{emptyFailover,kimchiModels,minimaxCodeUsage,opencodeTlsClient,projectId,usage,compact}.js`
and `src/lib/{consoleLogBuffer,headroom/**,pxpipe/**,tunnel/**,mcp/stdioSseBridge,appUpdater,updater/**,auth/dashboardSession,auth/loginLimiter,auth/trustedPeer,modelCatalog/**,network/**,mimoLoginSession,oauth/perchPlan,oauth/kiroExternalIdp}.js`.

### CLI

`cli/src/cli/commands/xaiVideo.js` is the only command file; the Rust binary has
`connect`, `show`, `update`, `xai video` — the last stubbed at the request layer.
`cli/src/cli/menus/**` and `cli/src/cli/tray/**` have no Rust equivalent.

## Notes on this pass

Landed by this pass:

| Group | Rust location | Status |
|---|---|---|
| `src/lib/consoleLogBuffer.js` | `nr-server/src/console_log_buffer.rs` | Ring buffer (maxLines 200, 100 ms batch flush, ANSI strip) + broadcast fan-out. |
| `translator/console-logs`, `…/console-logs/stream` | `nr-server/src/routes/translator.rs` | `GET`/`DELETE` + SSE (`init`/`lines`/`clear`, 25 s keepalive). |
| `translator/load`, `translator/save` | `nr-server/src/routes/translator.rs` | Allow-listed filenames only (path traversal rejected), 400/404 parity. |
| `oauth/codex/import-token` | `nr-server/src/routes/oauth_imports.rs` | Decodes the JWT for email/workspace/plan, stores `authType: "access_token"`. |
| `oauth/codex/bulk-import` | same | Serial inserts (priority race), strips server fields, per-item `results`. |
| `oauth/grok-cli/bulk-import` | same | snake_case + camelCase items, `decodeXaiIdTokenEmail` fallback chain. |
| `oauth/gitlab/pat` | same | Live `/api/v4/user` verification behind the SSRF guard. |
| `oauth/perchai/import` | same | Posted token or `~/.perch/cli-auth-session.json` fallback. |
| 6 × `oauth/*/auto-import` | `oauth_imports.rs` + `oauth_local_creds.rs` | commandcode, cursor (read-only SQLite), kiro (AWS SSO cache + profile ARN region normalisation), perchai (file/keychain), xiaomi-mimo, zed (`secret-tool`/`security`). |
| `src/lib/oauth/providerHelpers.js` JWT helpers | `nr-server/src/routes/oauth_jwt.rs` | `decodeJwtPayload`, `extractEmailFromAccessToken`, `extractCodexAccountInfo`. |
| headroom + pxpipe + tunnel + mcp routes | `nr-server/src/routes/{headroom,pxpipe,tunnel,mcp}.rs` | **Another agent's lane** — see below. |

Deliberately left at `501 NOT_IMPLEMENTED` in `oauth_imports.rs`, each with a
stable `code` so the dashboard can fall back to manual paste: `oauth/kiro/{import,
import-cli-proxy,api-key,social-authorize,social-exchange}`, `oauth/iflow/cookie`,
`oauth/zed/import`, `oauth/cursor/import`, `oauth/xiaomi-mimo/{api-key,
login/start,login/status}`, `oauth/perchai/plan/{status,claim}`. They need
validation helpers from `src/lib/oauth/services/**` and
`src/lib/oauth/{perchPlan,kiroExternalIdp}.js` that live in `nr-oauth` (owned by
another agent). Request-shape validation is ported and returns `400` first.

`headroom/**` (6 routes), `pxpipe/**` (8 routes), `tunnel/**` (7 routes) and
`mcp/[plugin]/*` (2 routes) are implemented by the `impl-routes-2` agent.
`translator/translate` and `translator/send` are that same agent's, appended to
`translator.rs`.

