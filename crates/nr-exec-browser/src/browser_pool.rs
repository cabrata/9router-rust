//! Browser pool — shared Chromium instance manager for web-cookie providers.
//!
//! Manages a pool of browser contexts, reusing Chromium instances across requests.
//! Handles idle timeouts, context eviction, and virtual display management.
//!
//! Port of open-sse/services/browserPool.js.
//! shortcut: CDP/Playwright automation not yet implemented; using stub for interface definition

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use parking_lot::RwLock;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

/// Browser context pooled for reuse
#[derive(Clone)]
pub struct BrowserContext {
    /// Unique pool key for this context
    pub id: String,
    /// Last access timestamp (ms)
    pub last_used: u64,
    /// Whether this context uses stealth mode (Obscura/cloakbrowser)
    pub is_stealth: bool,
}

/// Pooled context holder
struct PooledContext {
    context: BrowserContext,
    created_at: u64,
}

/// Browser pool metrics
#[derive(Clone, Debug)]
pub struct BrowserPoolMetrics {
    pub browser_launches: u32,
    pub browser_launch_failures: u32,
    pub contexts_created: u32,
    pub contexts_reused: u32,
    pub contexts_evicted: u32,
    pub contexts_released: u32,
    pub context_create_failures: u32,
    pub shutdowns: u32,
    pub last_shutdown_reason: Option<String>,
}

/// Browser pool status
#[derive(Clone, Debug)]
pub struct BrowserPoolStatus {
    pub enabled: bool,
    pub contexts: usize,
    pub browser_running: bool,
    pub engine: Option<String>,
    pub stealth_available: bool,
    pub last_activity_ago_ms: i64,
}

/// Global browser pool state
struct PoolState {
    contexts: HashMap<String, PooledContext>,
    pending_contexts: HashMap<String, Arc<Mutex<Option<BrowserContext>>>>,
    generation: u64,
    last_activity: u64,
    idle_timer: Option<JoinHandle<()>>,
    evict_timer: Option<JoinHandle<()>>,
    metrics: BrowserPoolMetrics,
}

lazy_static::lazy_static! {
    static ref POOL_STATE: RwLock<PoolState> = RwLock::new(PoolState {
        contexts: HashMap::new(),
        pending_contexts: HashMap::new(),
        generation: 0,
        last_activity: current_time_ms(),
        idle_timer: None,
        evict_timer: None,
        metrics: BrowserPoolMetrics {
            browser_launches: 0,
            browser_launch_failures: 0,
            contexts_created: 0,
            contexts_reused: 0,
            contexts_evicted: 0,
            contexts_released: 0,
            context_create_failures: 0,
            shutdowns: 0,
            last_shutdown_reason: None,
        },
    });
}

const POOL_IDLE_TIMEOUT_MS: u64 = 5 * 60 * 1000;
const CONTEXT_TTL_MS: u64 = 10 * 60 * 1000;

/// Options for acquiring a browser context
#[derive(Clone, Debug)]
pub struct AcquireContextOptions {
    pub cookie_string: Option<String>,
    pub cookie_domain: Option<String>,
    pub local_storage: Option<HashMap<String, String>>,
    pub local_storage_origin: Option<String>,
    pub warmup_url: Option<String>,
    pub user_agent: Option<String>,
    pub locale: Option<String>,
    pub timezone: Option<String>,
    pub headless: bool,
    pub executable_path: Option<String>,
}

impl Default for AcquireContextOptions {
    fn default() -> Self {
        Self {
            cookie_string: None,
            cookie_domain: None,
            local_storage: None,
            local_storage_origin: None,
            warmup_url: None,
            user_agent: None,
            locale: None,
            timezone: None,
            headless: true,
            executable_path: None,
        }
    }
}

/// Default user agent for browser contexts
pub const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36";

fn current_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Check if the browser pool is enabled (respects NINEROUTER_BROWSER_POOL env var)
pub fn is_pool_enabled() -> bool {
    match std::env::var("NINEROUTER_BROWSER_POOL") {
        Ok(val) => !matches!(val.as_str(), "off" | "0" | "false"),
        Err(_) => true,
    }
}

/// Acquire a browser context for the given key, reusing from pool if available
/// shortcut: actual browser automation not implemented; returns error for now
pub async fn acquire_browser_context(
    _key: &str,
    _options: AcquireContextOptions,
) -> nr_core::Result<BrowserContext> {
    if !is_pool_enabled() {
        return Err(nr_core::Error::other("browserPool: NINEROUTER_BROWSER_POOL=off — context requested but pool is disabled"));
    }
    // TODO(dedupe): Implement actual browser launch and context creation
    Err(nr_core::Error::other("browser automation not yet implemented"))
}

/// Release a browser context back to the pool (closes it immediately for now)
pub async fn release_browser_context(_key: &str) -> nr_core::Result<()> {
    // TODO(dedupe): Implement context reuse and proper cleanup
    Ok(())
}

/// Shutdown the entire browser pool
pub async fn shutdown_pool(reason: &str) -> nr_core::Result<()> {
    let mut state = POOL_STATE.write();
    state.generation += 1;
    state.metrics.shutdowns += 1;
    state.metrics.last_shutdown_reason = Some(reason.to_string());
    state.contexts.clear();
    state.pending_contexts.clear();
    // TODO(dedupe): Kill actual browser processes and cleanup
    Ok(())
}

/// Get current browser pool status
pub fn get_browser_pool_status() -> BrowserPoolStatus {
    let state = POOL_STATE.read();
    BrowserPoolStatus {
        enabled: is_pool_enabled(),
        contexts: state.contexts.len(),
        browser_running: false, // TODO(dedupe): check actual browser state
        engine: None,           // TODO(dedupe): track engine type
        stealth_available: false, // TODO(dedupe): check Obscura/cloakbrowser availability
        last_activity_ago_ms: (current_time_ms() as i64) - (state.last_activity as i64),
    }
}

/// Get browser pool metrics
pub fn get_browser_pool_metrics() -> (BrowserPoolStatus, BrowserPoolMetrics) {
    let state = POOL_STATE.read();
    (get_browser_pool_status(), state.metrics.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pool_enabled_by_default() {
        assert!(is_pool_enabled());
    }

    #[test]
    fn test_default_acquire_options() {
        let opts = AcquireContextOptions::default();
        assert!(opts.headless);
        assert_eq!(opts.user_agent, None);
    }

    #[tokio::test]
    async fn test_acquire_context_fails_when_disabled() {
        std::env::set_var("NINEROUTER_BROWSER_POOL", "off");
        let result = acquire_browser_context("test-key", AcquireContextOptions::default()).await;
        assert!(result.is_err());
        std::env::remove_var("NINEROUTER_BROWSER_POOL");
    }
}
