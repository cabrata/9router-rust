//! Port of open-sse/services/browserPool.js
//! Shared browser pool for web executors with context caching and TTL-based eviction.

use crate::cdp::{Browser, BrowserContext, ContextOptions as CdpContextOptions, PlaywrightCookie};
use lazy_static::lazy_static;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

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
    /// Cookie string ("a=1; b=2") installed before the first page loads.
    pub cookie_string: Option<String>,
    /// Domain those cookies belong to (DEFAULT: none → cookie needs no domain).
    pub cookie_domain: Option<String>,
    /// `localStorage` entries seeded on `local_storage_origin`.
    pub local_storage: Option<HashMap<String, String>>,
    pub local_storage_origin: Option<String>,
    /// Page that must load once before the context is handed out.
    pub warmup_url: Option<String>,
}

impl Default for ContextOptions {
    fn default() -> Self {
        Self {
            user_agent: None,
            locale: None,
            timezone_id: None,
            headless: None,
            cookie_string: None,
            cookie_domain: None,
            local_storage: None,
            local_storage_origin: None,
            warmup_url: None,
        }
    }
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
    /// Last time any context was acquired or released (ms).
    last_activity: u64,
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
    pub shutdowns: u64,
    pub last_shutdown_reason: Option<String>,
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
            shutdowns: 0,
            last_shutdown_reason: None,
        }
    }
}

lazy_static! {
    static ref POOL_STATE: RwLock<PoolState> = RwLock::new(PoolState {
        browser: None,
        contexts: HashMap::new(),
        metrics: PoolMetrics::default(),
        last_activity: 0,
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

    // Expire TTL'd contexts and an idle browser before handing anything out.
    evict_stale(now_ms()).await;

    let pool_key = format!("{}:{}", if options.headless.unwrap_or(true) { "headless" } else { "headed" }, key);

    // Reuse a live context; drop a stale one so its slot is recreated.
    let now = now_ms();
    {
        let mut state = POOL_STATE.write();
        // Take the handle first so `state` is free for the metric updates.
        let reused = state.contexts.get_mut(&pool_key).and_then(|pooled| {
            (now.saturating_sub(pooled.created_at) < CONTEXT_TTL_MS).then(|| {
                pooled.last_used = now;
                pooled.context.clone()
            })
        });
        if let Some(context) = reused {
            state.metrics.contexts_reused += 1;
            state.last_activity = now;
            return Ok(context);
        }
        // TTL expired (or none yet): evict before creating the replacement.
        if state.contexts.remove(&pool_key).is_some() {
            state.metrics.contexts_evicted += 1;
        }
    }

    let browser = match get_or_launch_browser(options.headless.unwrap_or(true)).await {
        Ok(b) => b,
        Err(err) => {
            POOL_STATE.write().metrics.browser_launch_failures += 1;
            return Err(err);
        }
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
            state.last_activity = now_ms();
            Ok(context)
        }
        Err(e) => {
            POOL_STATE.write().metrics.context_create_failures += 1;
            Err(BrowserPoolError::ContextCreateFailed(e))
        }
    }
}

/// Return the shared Chromium, launching it on first use.
///
/// A dead browser (crashed, or closed by `shutdown_pool`) is dropped and
/// relaunched so a transient crash does not wedge the pool.
async fn get_or_launch_browser(headless: bool) -> Result<Arc<dyn Browser>, BrowserPoolError> {
    // Serialize launches so parallel first requests share one process.
    static LAUNCH_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = LAUNCH_GUARD.lock().await;

    {
        let mut state = POOL_STATE.write();
        match &state.browser {
            Some(browser) if browser.is_connected() => return Ok(browser.clone()),
            Some(_) => {
                state.browser = None;
                state.contexts.clear();
            }
            None => {}
        }
    }

    let executable = crate::browser_executable_check::resolve_system_chrome_executable();
    let launched = crate::cdp::CdpBrowser::launch(executable.as_deref(), headless)
        .await
        .map_err(|e| {
            if crate::browser_executable_check::is_missing_browser_executable(&e)
                || crate::browser_executable_check::is_missing_display(&e)
            {
                // Host/config problems surface verbatim so the dashboard can
                // tell the user to install Chrome instead of retrying.
                BrowserPoolError::BrowserNotAvailable
            } else {
                BrowserPoolError::ContextCreateFailed(e)
            }
        })?;

    let browser: Arc<dyn Browser> = launched;
    let mut state = POOL_STATE.write();
    state.browser = Some(browser.clone());
    state.metrics.browser_launches += 1;
    Ok(browser)
}

/// Release a browser context back to the pool.
///
/// The context stays available for reuse (matching the JS pool, which only
/// closes on TTL eviction or shutdown); the caller drops its handle.
pub async fn release_browser_context(context: Arc<dyn BrowserContext>) {
    let mut state = POOL_STATE.write();
    state.metrics.contexts_released += 1;
    // Bump last_used so the eviction pass gives the context a fresh window.
    let now = now_ms();
    state.last_activity = now;
    for pooled in state.contexts.values_mut() {
        if Arc::ptr_eq(&pooled.context, &context) {
            pooled.last_used = now;
        }
    }
}

/// Evict contexts older than [`CONTEXT_TTL_MS`] and idle browsers past
/// [`POOL_IDLE_TIMEOUT_MS`]. Called before acquisitions, so no timer thread is
/// needed.
async fn evict_stale(now: u64) {
    let stale: Vec<Arc<dyn BrowserContext>> = {
        let mut state = POOL_STATE.write();
        let expired: Vec<String> = state
            .contexts
            .iter()
            .filter(|(_, pooled)| now.saturating_sub(pooled.created_at) >= CONTEXT_TTL_MS)
            .map(|(key, _)| key.clone())
            .collect();
        let mut removed = Vec::new();
        for key in expired {
            if let Some(pooled) = state.contexts.remove(&key) {
                state.metrics.contexts_evicted += 1;
                removed.push(pooled.context);
            }
        }
        removed
    };

    for context in stale {
        let _ = context.close().await;
    }

    // An idle pool releases the browser so a laptop does not keep Chromium
    // resident forever.
    let idle_browser = {
        let mut state = POOL_STATE.write();
        if state.contexts.is_empty() {
            let idle = state
                .contexts
                .values()
                .map(|p| p.last_used)
                .max()
                .unwrap_or(0);
            let last_activity = state.last_activity.max(idle);
            if now.saturating_sub(last_activity) >= POOL_IDLE_TIMEOUT_MS {
                state.browser.take()
            } else {
                None
            }
        } else {
            None
        }
    };
    if let Some(browser) = idle_browser {
        let _ = browser.close().await;
    }
}

/// Get browser pool status
pub fn get_browser_pool_status() -> BrowserPoolStatus {
    let state = POOL_STATE.read();
    let connected = state.browser.as_ref().map(|b| b.is_connected()).unwrap_or(false);
    let last_activity = state
        .contexts
        .values()
        .map(|p| p.last_used)
        .max()
        .unwrap_or(state.last_activity);
    BrowserPoolStatus {
        enabled: is_pool_enabled(),
        contexts: state.contexts.len(),
        browser_running: connected,
        // Only a launched local Chromium exists in this port; Obscura's CDP
        // endpoint is reachable but not the pool engine yet.
        engine: connected.then(|| "chromium".to_string()),
        stealth_available: false,
        last_activity_ago_ms: if last_activity == 0 { 0 } else { (now_ms() as i64) - (last_activity as i64) },
    }
}

/// Get browser pool metrics
pub fn get_browser_pool_metrics() -> PoolMetrics {
    POOL_STATE.read().metrics.clone()
}

/// Shutdown the browser pool: close every context, then the browser process.
pub async fn shutdown_pool(reason: &str) {
    tracing::info!("Shutting down browser pool: {}", reason);
    let (contexts, browser) = {
        let mut state = POOL_STATE.write();
        state.metrics.shutdowns += 1;
        state.metrics.last_shutdown_reason = Some(reason.to_string());
        let contexts: Vec<Arc<dyn BrowserContext>> =
            state.contexts.drain().map(|(_, pooled)| pooled.context).collect();
        (contexts, state.browser.take())
    };

    for context in contexts {
        let _ = context.close().await;
    }
    if let Some(browser) = browser {
        // `Browser.close` + SIGKILL fallback: the Chromium child is
        // `kill_on_drop`, so a stuck process is reaped either way.
        let _ = browser.close().await;
    }
}

async fn create_context(
    browser: &Arc<dyn Browser>,
    options: ContextOptions,
) -> Result<Arc<dyn BrowserContext>, String> {
    let ContextOptions {
        user_agent,
        locale,
        timezone_id,
        headless,
        cookie_string,
        cookie_domain,
        local_storage,
        local_storage_origin,
        warmup_url,
    } = options;

    let cdp_options = CdpContextOptions {
        user_agent: user_agent.or(Some(DEFAULT_USER_AGENT.to_string())),
        locale,
        timezone_id,
        headless,
    };
    let context = browser.new_context(cdp_options).await?;

    // Cookies are installed on the context so every page created later carries
    // the authenticated session (the JS pool does the same before `newPage`).
    if let Some(raw) = cookie_string.filter(|c| !c.trim().is_empty()) {
        let cookies = match cookie_domain.as_deref() {
            Some(domain) if !domain.is_empty() => parse_cookie_string(&raw, domain),
            _ => parse_cookie_string_without_domain(&raw),
        };
        if !cookies.is_empty() {
            context.add_cookies(&cookies).await?;
        }
    }

    if let (Some(origin), Some(entries)) = (local_storage_origin.as_deref(), local_storage.as_ref()) {
        if !entries.is_empty() {
            let script = format!(
                "(() => {{ localStorage.setItem({}, {}); return true; }})()",
                serde_json::to_string("__9router_seed__").unwrap_or_else(|_| "\"\"".into()),
                serde_json::to_string(&serde_json::to_string(entries).unwrap_or_default())
                    .unwrap_or_else(|_| "\"\"".into())
            );
            // Seed on every page load of that origin, then expand the entries.
            let seeds: Vec<String> = entries
                .iter()
                .map(|(k, v)| {
                    format!(
                        "localStorage.setItem({}, {});",
                        serde_json::to_string(k).unwrap_or_else(|_| "\"\"".into()),
                        serde_json::to_string(v).unwrap_or_else(|_| "\"\"".into())
                    )
                })
                .collect();
            let init = format!("if (location.origin === {:?}) {{ {} }}", origin, seeds.join(" "));
            context.add_init_script(&init, None).await?;
            let _ = script;
        }
    }

    // A warmup navigation makes the first real request cheap and surfaces a
    // dead session (login redirect) before the executor starts typing.
    if let Some(url) = warmup_url.filter(|u| !u.trim().is_empty()) {
        let page = context.new_page().await?;
        let goto = crate::cdp::GotoOptions { wait_until: "domcontentloaded".into(), timeout_ms: 20_000 };
        let result = page.goto(&url, goto).await;
        let _ = page.close().await;
        result?;
    }

    Ok(context)
}

/// Cookies for a context without a domain (CDP lets the browser infer one).
pub fn parse_cookie_string_without_domain(raw: &str) -> Vec<PlaywrightCookie> {
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
                domain: None,
                path: Some("/".to_string()),
                expires: None,
                http_only: Some(false),
                secure: Some(true),
                same_site: Some("Lax".to_string()),
            })
        })
        .collect()
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
