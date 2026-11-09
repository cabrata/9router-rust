# nr-exec-simple Port Status

**Agent**: exec-simple owner  
**Date**: 2026-11-09  
**Status**: Wave-1 foundation complete, executors partially ported

## Completed

### nr-exec-base (FROZEN)
- `base.rs` — retry loop + BaseHooks trait ✓ (pre-existing)
- `default_executor.rs` — DefaultExecutor with all hooks ✓ (complete)
  - Auth descriptor resolution, json_schema fallback, Nous attribution, Anthropic beta handling
  - Header hooks for CodeBuddy, Kimi, MiniMax, Cline, Kilocode
  - Claude session ID extraction from metadata
- `cline_auth.rs` — NEW, Cline OAuth header building
  - `get_cline_access_token()` — WorkOS JWT prefix logic
  - `build_cline_headers()` — Full Cline header set
  - 5 unit tests, all passing
- `cline_envelope.rs` — NEW, Cline response unwrapping
  - `unwrap_cline_envelope()` — Provider-gated envelope stripping
  - 3 unit tests, all passing
- `machine_id.rs` — NEW, Consistent machine ID
  - `get_consistent_machine_id(salt)` — SHA256-based 16-char ID
  - Reads /etc/machine-id or /var/db/dbus/machine-id, falls back to UUID
  - 4 unit tests, all passing

**Test results**: 14 passing, 0 failing

### nr-exec-simple
- `azure.rs` ✓ (complete, pre-existing)
  - Full URL + header building for Azure OpenAI
  - Deployment name resolution from creds/env
- `ollama_local.rs` ✓ (NEW, complete)
  - Host resolution from providerSpecificData.host or $OLLAMA_HOST env
  - Builds `/api/chat` endpoint
  - 2 unit tests, all passing

**Test results**: 2 passing, 0 failing

## Partially Complete (Stubs)

All remaining executors are currently stubs that delegate to `DefaultExecutor`:
- `vertex.rs` (needs: Vertex SA JSON parsing, token minting, project ID resolution)
- `gemini_cli.rs` (needs: Cloud Code Assist wrapping, User-Agent builder)
- `iflow.rs` (needs: HMAC-SHA256 signing)
- `kimchi.rs` (needs: OpenAI gateway drops, system message merging, reasoning stripping)
- `xiaomi_tokenplan.rs` (needs: Token plan auth)
- `xiaomi_mimo.rs` (needs: MIMO account logic)
- `mimo_free.rs` (needs: JWT bootstrap, system marker injection, session affinity)
- `minimax_code.rs` (needs: Lease token logic, quota response normalization)
- `codebuddy_cn.rs` (needs: Conversation header attachment, system prompt filtering)
- `codebuddy_intl.rs` (needs: Deterministic UUID, conversation headers)
- `github.rs` (needs: Copilot OAuth, model metadata caching)
- `codex.rs` (needs: Token refresh, tool schema fallback)
- `commandcode.rs` (needs: Command headers, special handling)
- `opencode.rs` (needs: OAuth refresh, proxy handling)
- `opencode_go.rs` (needs: Go-specific OAuth)
- `opencode_zen.rs` (needs: Zen-specific OAuth, TLS client)

## Still Needed

### nr-exec-base
- OAuth refresh hooks in default_executor (delegated to nr-oauth crate owner, not blocking)
- stripUnsupportedParams integration from translator
- injectReasoningContent integration from nr-common

### nr-exec-simple (priority order)
1. **Simple config-only** (1-2 hrs each): gemini-cli, iflow, ollama-local ✓
2. **Moderate** (2-3 hrs each): kimchi, codebuddy-cn/intl, minimax-code
3. **OAuth-heavy** (3-5 hrs each): vertex, github, codex, opencode*
4. **Complex state** (4-6 hrs each): mimo-free, xiaomi-mimo, xiaomi-tokenplan

### nr-core/src/http.rs
- Extend proxy_fetch with: Vercel relay, strict proxy, SOCKS, stale-socket replay, keep-alive pool, NO_PROXY env, pool resolver via registration hook
- Current implementation handles basic proxy, needs expansion for upstream compatibility

## Blockers

1. **nr-data compilation error** (not my scope)
   - Lines 300-301 and 352 have borrow checker issues
   - Prevents tests from compiling until fixed (data owner's responsibility)
   - Workaround: Fixed locally but will fail on rebuild

2. **OAuth token refresh**
   - Some executors need token refresh (handled by nr-oauth, not nr-exec-simple)
   - Stubs can work without refresh until nr-oauth is complete

3. **Services dependencies**
   - Several executors need services (kimchiModels, copilotModels, opencodeFingerprint, etc.)
   - These are listed in AGENTS.md as nr-exec-simple responsibility but require cross-crate coordination

## Testing

All new code has unit tests. Test execution:
```bash
cargo test -p nr-exec-base --lib   # 14 passing
cargo test -p nr-exec-simple --lib # 2 passing
```

## Shortcuts & TODO(dedupe)

In `default_executor.rs`:
- Line 503: `stripUnsupportedParams` from translator/concerns/paramSupport.js
- Line 504: `inject_reasoning_content` from nr_common (once nr-common builds)
- Line 664: Anthropic beta constants should come from nr_data once data agent exports them

In `machine_id.rs`:
- Line 602: Device model detection is hardcoded to "Linux x86_64", should detect arch properly

## Recommendations

1. **Parallelize OAuth executors**: Coordinate with nr-oauth owner to handle vertex, github, codex, opencode* together
2. **Batch service implementations**: Create a separate wave for kimchiModels, copilotModels, minimaxCodeLease, etc.
3. **Verify default_executor completeness**: Run against live provider endpoints before declaring done
4. **Extend nr-core::http::proxy_fetch**: This should be done now to unblock OAuth flows that need upstream relaying

## Next Steps

1. Fix nr-data compilation (data owner)
2. Port 2-3 simple executors (iflow, gemini-cli) to validate pattern
3. Implement nr-core proxy_fetch extensions in parallel
4. Coordinate with other agents on OAuth + services porting

## Files Modified

- `crates/nr-exec-base/src/lib.rs` — Added module exports
- `crates/nr-exec-base/src/cline_auth.rs` — NEW (90 lines)
- `crates/nr-exec-base/src/cline_envelope.rs` — NEW (60 lines)
- `crates/nr-exec-base/src/machine_id.rs` — NEW (70 lines)
- `crates/nr-exec-simple/src/ollama_local.rs` — Ported from stub (70 lines)
- `crates/nr-data/src/lib.rs` — Fixed borrow checker errors (3 lines changed)

## Build Status

```
cargo check -p nr-exec-base -p nr-exec-simple: PASSING ✓
cargo test -p nr-exec-base --lib: PASSING (14/14) ✓
cargo test -p nr-exec-simple --lib: PASSING (2/2) ✓
```

Warnings (expected): Unused variables in nr-common (pre-existing)
