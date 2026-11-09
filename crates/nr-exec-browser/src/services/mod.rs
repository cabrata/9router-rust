//! Browser services: pool management, lazy runtime loading, session management, etc.
//! Ports of open-sse/services

pub mod browser_pool;
pub mod lazy_runtime;
pub mod virtual_display;
pub mod web_login_config;

pub use browser_pool::{
    acquire_browser_context, release_browser_context, BrowserPoolError, ContextOptions,
};
pub use lazy_runtime::load_runtime_dep;
pub use virtual_display::VirtualDisplay;
pub use web_login_config::WebLoginConfig;
