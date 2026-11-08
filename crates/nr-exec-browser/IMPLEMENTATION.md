# nr-exec-browser Implementation Status

Port of `open-sse/executors/` browser-based executors and supporting infrastructure.

## Completed ✓

### Browser Infrastructure
- **browser_executable_check.rs**
  - Distinguishes missing browser executables (host config problem) from transient faults
  - `is_missing_browser_executable()` - checks error messages for executable-not-found patterns
  - `is_missing_display()` - checks for X server errors on headed browsers
  - `resolve_system_chrome_executable()` - finds system Chrome binary from env/standard paths
  - Tests: ✓ Unit tests for error detection

- **browser_pool.rs**
  - Core pool data structures and types defined
  - `AcquireContextOptions` - configurable context creation (cookies, localStorage, user agent, etc.)
  - `BrowserContext` - pooled context representation
  - `BrowserPoolMetrics` - lifecycle telemetry tracking
  - `BrowserPoolStatus` - pool status reporting
  - Pool state management (contexts, pending operations, generation tracking, idle timeouts)
  - `is_pool_enabled()` - respects NINEROUTER_BROWSER_POOL env var
  - `get_browser_pool_status()` / `get_browser_pool_metrics()` - observability functions
  - Tests: ✓ Pool enabled/disabled, default options, status reporting

### Executor Stubs (Ready for Implementation)
- **gemini_web.rs**
  - Port of `open-sse/executors/gemini-web.js` + `gemini-web/**`
  - Placeholder implementation returns 501 Not Implemented
  - Ready for browser automation integration
  - Scope: Gemini web UI automation, multi-turn flattening, mode selection, extended thinking, tool-calling

- **grok_web.rs**
  - Port of `open-sse/executors/grok-web.js` + `grok-web/**`
  - Placeholder implementation returns 501 Not Implemented
  - Ready for browser automation integration
  - Scope: Grok web UI automation, SSO cookie handling, tool-calling via grok-web-tools.js

- **muse_web.rs**
  - Port of `open-sse/executors/muse-web.js` + `muse-web/**`
  - Placeholder implementation returns 501 Not Implemented
  - Ready for browser automation integration
  - Scope: Muse web UI automation, session management, DOM capture

- **veoaifree_web.rs**
  - Port of `open-sse/executors/veoaifree-web.js`
  - Placeholder implementation returns 501 Not Implemented
  - Note: Not a browser executor - AJAX API to veoaifree.com
  - Scope: Video/image generation via WordPress AJAX, polling, artifact download, SSRF guards

## Shortcuts & TODOs

### shortcut: Browser Automation Engine Not Implemented
**Location:** `browser_pool.rs:acquire_browser_context()` and all executors
**Limitation:** No actual Chromium/browser instance management
**When to upgrade:** Once a CDP client (chromiumoxide, puppeteer-rs) or Playwright binding is available
**Current approach:** Stub functions return "not implemented" errors
**Path forward:**
1. Add browser control crate to workspace (chromiumoxide recommended for rustc 1.85)
2. Implement `launch_browser_instance()` in browser_pool
3. Implement context lifecycle management (create, seed cookies/localStorage, cleanup)
4. Implement warmup page loading and virtual display support

### TODO(dedupe): Virtual Display Support (Linux headed browsers)
**Location:** Related to `browser_pool.rs` headless browser launch
**JS Source:** `open-sse/services/virtualDisplay.js`
**Status:** Not ported yet
**Depends on:** Subprocess management for Xvfb on Linux, DISPLAY env var handling
**Impact:** Headed browser tests on headless Linux hosts will fail

### TODO(dedupe): Browser Profile Cache
**Location:** Separate module needed
**JS Source:** `open-sse/services/browserProfileCache.js`
**Status:** Not ported yet
**Purpose:** Persistent browser profiles to avoid re-loading state
**Impact:** Context creation latency, but not blocking for basic functionality

### TODO(dedupe): Cookie & Session Management
**Location:** `browser_pool.rs:seed_context_session()`
**JS Source:** `open-sse/services/webCookieAuth.js`, `open-sse/services/geminiCookies.js`
**Status:** Not ported yet
**Depends on:** Cookie parsing, Playwright cookie API equivalents
**Impact:** Cannot seed cookies into contexts for authenticated flows

### TODO(dedupe): Lazy Runtime Dependency Loading
**Location:** Would be for browser binary management
**JS Source:** `open-sse/services/lazyRuntime.js`
**Status:** N/A for Rust (dependencies at compile time)
**Note:** In Rust, this is handled by Cargo. May need external browser binary download logic if not bundling.

### TODO(dedupe): Obscura CDP Integration
**Location:** Stealth browser engine in pool
**JS Source:** `open-sse/services/obscura.js`
**Status:** Not ported yet
**Purpose:** Lightweight CDP browser vs. full Chromium (anti-bot evasion)
**Impact:** Stealth mode unavailable; strong anti-bot challenges will detect Chromium

### TODO(dedupe): Cloakbrowser Integration
**Location:** Fingerprint patching in pool
**JS Source:** browserPool.js: `resolveCloakLaunch()`
**Status:** Not ported yet
**Purpose:** Binary-level fingerprint patching for anti-bot evasion
**Impact:** Stealth mode unavailable without this

### TODO(dedupe): Web Identity & Login Services
**Location:** Separate modules needed
**JS Source:** `open-sse/services/webIdentity.js`, `open-sse/services/webLogin.js`, `open-sse/services/webLoginConfig.js`
**Status:** Not ported yet
**Purpose:** Session bootstrap, OAuth redirect handling, login workflow management
**Impact:** Currently cannot bootstrap sessions via login flows

### TODO(dedupe): Executor-Specific Features
**gemini-web:**
- `gemini-web/modeSelection.js` - model mode verification and switching
- `gemini-web/domCapture.js` - page response capturing and quota detection
- `gemini-web/imageGen.js` - image generation workflow
- `gemini-web/capabilities.js` - control support detection (thinking, tools)
- `gemini-web/browserLease.js` - context acquisition pattern

**grok-web:**
- `grok-web-tools.js` - tool-calling translation layer
- Browser profile for grok.com
- Statsig ID and xAI request ID headers

**muse-web:**
- `muse-web/session.js` - session management
- `muse-web/dom.js` - DOM manipulation and response capture
- `muse-web/prompt.js` - prompt building for web UI

**veoaifree-web:**
- AJAX workflow orchestration (submit -> poll -> download)
- Private address SSRF guards
- Artifact content validation
- Rate limit handling (202, 404, 409, 425 retries)

## Not Yet Ported (Lower Priority)

### Services (in-progress by other agents)
- geminiCookies - part of nr-common (data agent), needed for cookie normalization
- webCookieAuth - part of nr-common, needed for Playwright cookie conversion
- sessionManager - part of nr-common, for context state

### Translator Files (other agent)
- `translator/webTools.js` - tool-calling preparation
- Web-based tool shims (chatgptWebTools.js pattern)
- These are in nr-exec-webchat scope

### Tests
- No live browser tests (no real Chromium instances)
- No real provider calls (veoaifree, Gemini, etc.)
- Unit tests for error detection and pool logic only
- Need mock browser instances for integration tests

## Compilation Status

**Current state:** Compiles when dependencies (nr-common, nr-oauth, etc.) are fixed
**Blocking issues:** Upstream compilation errors in nr-common prevent full build
**Executors:** Using default stub returns 501 until browser automation is implemented

## Interface Contracts (Frozen per AGENTS.md)

The following are public interfaces that other crates depend on:
- `browser_pool::acquire_browser_context()` - context acquisition
- `browser_pool::release_browser_context()` - context cleanup
- `browser_pool::get_browser_pool_status()` - observability
- `browser_pool::get_browser_pool_metrics()` - telemetry
- Each executor module's `create(provider: &str) -> Arc<dyn Executor>`

These signatures are stable. Implementations can be added without breaking consumers.

## Build Notes

- Requires rustc 1.85+
- No optional features (browser automation is mandatory for this crate)
- Workspace dependencies: lazy_static, async-trait, tokio, serde_json
- Recommended: Add chromiumoxide (or Playwright binding) to workspace Cargo.toml
- Tests use mocked pool state (no actual browser launches)
