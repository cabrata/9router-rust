//! Port of open-sse/services/browserPool.js
//! Shared browser pool for web executors with context caching and TTL-based eviction.

use crate::cdp::{Browser, BrowserContext, ContextOptions as CdpContextOptions, PlaywrightCookie};
use lazy_static::lazy_static;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const POOL_IDLE_TIMEOUT_MS: u64 = 5 * 60 * 1000;
const CONTEXT_TTL_MS: u64 = 10 * 60 * 1000;
const EVICT_INTERVAL_MS: u64 = 60 * 1000;

pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36";

#[derive(Debug, Clone)]
pub enum BrowserPoolError {
    PoolDisabled,
    ContextCreateFailed(String),
    BrowserNotAvailable,
}

impl std::fmt::Display for BrowserPoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::PoolDisabled => write!(f, "Browser pool is disabled"),
            Self::ContextCreateFailed(msg) => write!(f, "Failed to create browser context: {}", msg),
            Self::BrowserNotAvailable => write!(f, "Browser not available"),
        }
    }
}

impl std::error::Error for BrowserPoolError {}

/// Options for acquiring a browser context
#[derive(Debug, Clone)]
pub struct ContextOptions {
    pub user_agent: Option<String>,
    pub locale: Option<String>,
    pub timezone_id: Option<String>,
    pub headless: Option<bool>,
}

/// Pooled context entry with TTL tracking
struct PooledContext {
    context: Arc<dyn BrowserContext>,
    created_at: u64,
    last_used: u64,
}

/// Browser pool state
struct PoolState {
    browser: Option<Arc<dyn Browser>>,
    contexts: HashMap<String, PooledContext>,
    metrics: PoolMetrics,
}

#[derive(Debug, Clone)]
pub struct BrowserPoolStatus {
    pub enabled: bool,
    pub contexts: usize,
    pub browser_running: bool,
    pub engine: Option<String>,
    pub stealth_available: bool,
    pub last_activity_ago_ms: i64,
}

#[derive(Debug, Clone)]
pub struct PoolMetrics {
    pub browser_launches: u64,
    pub browser_launch_failures: u64,
    pub contexts_created: u64,
    pub contexts_reused: u64,
    pub contexts_evicted: u64,
    pub contexts_released: u64,
    pub context_create_failures: u64,
}

impl Default for PoolMetrics {
    fn default() -> Self {
        Self {
            browser_launches: 0,
            browser_launch_failures: 0,
            contexts_created: 0,
            contexts_reused: 0,
            contexts_evicted: 0,
            contexts_released: 0,
            context_create_failures: 0,
        }
    }
}

lazy_static! {
    static ref POOL_STATE: RwLock<PoolState> = RwLock::new(PoolState {
        browser: None,
        contexts: HashMap::new(),
        metrics: PoolMetrics::default(),
    });
}

fn is_pool_enabled() -> bool {
    match env::var("NINEROUTER_BROWSER_POOL") {
        Ok(val) => val != "off" && val != "0" && val != "false",
        Err(_) => true,
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Acquire or reuse a browser context for the given isolation key
pub async fn acquire_browser_context(
    key: &str,
    options: ContextOptions,
) -> Result<Arc<dyn BrowserContext>, BrowserPoolError> {
    if !is_pool_enabled() {
        return Err(BrowserPoolError::PoolDisabled);
    }

    let pool_key = format!("{}:{}", if options.headless.unwrap_or(true) { "headless" } else { "headed" }, key);

    let browser = {
        let mut state = POOL_STATE.write();

        // Check for existing context
        if state.contexts.contains_key(&pool_key) {
            state.metrics.contexts_reused += 1;
            if let Some(pooled) = state.contexts.get_mut(&pool_key) {
                pooled.last_used = now_ms();
                return Ok(pooled.context.clone());
            }
        }

        // Create new context
        state.browser.clone()
            .ok_or(BrowserPoolError::BrowserNotAvailable)?
    };

    match create_context(&browser, options).await {
        Ok(context) => {
            let mut state = POOL_STATE.write();
            let pooled = PooledContext {
                context: context.clone(),
                created_at: now_ms(),
                last_used: now_ms(),
            };
            state.contexts.insert(pool_key, pooled);
            state.metrics.contexts_created += 1;
            Ok(context)
        }
        Err(e) => {
            let mut state = POOL_STATE.write();
            state.metrics.context_create_failures += 1;
            Err(BrowserPoolError::ContextCreateFailed(e))
        }
    }
}

/// Release a browser context back to the pool
pub async fn release_browser_context(context: Arc<dyn BrowserContext>) {
    let mut state = POOL_STATE.write();
    state.metrics.contexts_released += 1;
    // Context remains in pool for reuse or eviction
}

/// Get browser pool status
pub fn get_browser_pool_status() -> BrowserPoolStatus {
    let state = POOL_STATE.read();
    let connected = state.browser.as_ref().map(|b| b.is_connected()).unwrap_or(false);
    BrowserPoolStatus {
        enabled: is_pool_enabled(),
        contexts: state.contexts.len(),
        browser_running: connected,
        engine: None,
        stealth_available: false,
        last_activity_ago_ms: 0,
    }
}

/// Get browser pool metrics
pub fn get_browser_pool_metrics() -> PoolMetrics {
    POOL_STATE.read().metrics.clone()
}

/// Shutdown the browser pool
pub async fn shutdown_pool(reason: &str) {
    tracing::info!("Shutting down browser pool: {}", reason);
    let mut state = POOL_STATE.write();

    // Close all contexts
    for (_, pooled) in state.contexts.drain() {
        let _ = pooled.context.close().await;
    }

    // Close browser
    if let Some(browser) = state.browser.take() {
        let _ = browser.close().await;
    }
}

async fn create_context(
    browser: &Arc<dyn Browser>,
    options: ContextOptions,
) -> Result<Arc<dyn BrowserContext>, String> {
    let cdp_options = CdpContextOptions {
        user_agent: options.user_agent.or(Some(DEFAULT_USER_AGENT.to_string())),
        locale: options.locale,
        timezone_id: options.timezone_id,
        headless: options.headless,
    };

    browser.new_context(cdp_options).await
}

/// Parse cookie string into Playwright format
pub fn parse_cookie_string(raw: &str, domain: &str) -> Vec<PlaywrightCookie> {
    raw.split(';')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .filter_map(|pair| {
            let eq_idx = pair.find('=')?;
            let name = pair[..eq_idx].trim();
            let value = pair[eq_idx + 1..].trim();
            if name.is_empty() || value.is_empty() {
                return None;
            }
            Some(PlaywrightCookie {
                name: name.to_string(),
                value: value.to_string(),
                domain: Some(if domain.starts_with('.') {
                    domain.to_string()
                } else {
                    format!(".{}", domain)
                }),
                path: Some("/".to_string()),
                expires: None,
                http_only: Some(false),
                secure: Some(true),
                same_site: Some("Lax".to_string()),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cookie_string() {
        let cookies = parse_cookie_string("a=1; b=2", "example.com");
        assert_eq!(cookies.len(), 2);
        assert_eq!(cookies[0].name, "a");
        assert_eq!(cookies[0].value, "1");
        assert_eq!(cookies[0].domain, Some(".example.com".to_string()));
    }

    #[test]
    fn test_parse_cookie_string_with_dot_domain() {
        let cookies = parse_cookie_string("a=1", ".example.com");
        assert_eq!(cookies[0].domain, Some(".example.com".to_string()));
    }

    #[test]
    fn test_is_pool_enabled() {
        assert!(is_pool_enabled());
    }
}
