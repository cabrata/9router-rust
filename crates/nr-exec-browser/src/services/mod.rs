//! Browser services: pool management, lazy runtime loading, session management, etc.
//! Ports of open-sse/services

pub mod browser_pool;
pub mod browser_profile_cache;
pub mod lazy_runtime;
pub mod obscura;
pub mod virtual_display;
pub mod web_identity;
pub mod web_login_config;

pub use browser_profile_cache::{format_bytes, prune_profile_caches, PruneResult};
pub use browser_pool::{
    acquire_browser_context, release_browser_context, get_browser_pool_status,
    get_browser_pool_metrics, BrowserPoolError, BrowserPoolStatus, ContextOptions,
    PoolMetrics, DEFAULT_USER_AGENT,
};
pub use lazy_runtime::load_runtime_dep;
pub use obscura::{
    connect_obscura_browser, ensure_obscura_server, is_obscura_usable,
    kill_shared_obscura_server, ObscuraServer,
};
pub use virtual_display::VirtualDisplay;
pub use web_identity::{
    cookie_jar, decode_jwt_payload, fetch_web_identity, has_web_identity_probe, WebIdentity,
    GROK_TLS_USER_AGENT,
};
pub use web_login_config::WebLoginConfig;
