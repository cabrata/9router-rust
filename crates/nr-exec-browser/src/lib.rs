//! Specialized executors — port of open-sse/executors. Ownership: see AGENTS.md.
//!
//! Each executor module exposes `pub fn create(provider: &str) -> Arc<dyn Executor>`.

pub mod browser_executable_check;
pub mod cdp;
pub mod gemini_web;
pub mod grok_web;
pub mod muse_web;
pub mod veoaifree_web;
pub mod services;

// Re-export commonly used browser infrastructure
pub use browser_executable_check::{
    is_missing_browser_executable, is_missing_display,
    resolve_system_chrome_executable,
};
pub use services::{
    acquire_browser_context, release_browser_context,
    get_browser_pool_status, get_browser_pool_metrics,
    ContextOptions, DEFAULT_USER_AGENT,
};
pub use cdp::{Browser, BrowserContext, Page};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_browser_executable_check_missing() {
        assert!(browser_executable_check::is_missing_browser_executable(
            "Executable doesn't exist at /path"
        ));
        assert!(!browser_executable_check::is_missing_browser_executable(
            "Network error"
        ));
    }

    #[test]
    fn test_browser_executable_check_display() {
        assert!(browser_executable_check::is_missing_display(
            "No Xserver found"
        ));
        assert!(!browser_executable_check::is_missing_display(
            "Connection timeout"
        ));
    }
}
