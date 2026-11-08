#[cfg(test)]
mod browser_pool_tests {
    use crate::browser_pool::*;

    #[test]
    fn test_browser_pool_enabled_by_default() {
        // Reset env var to default
        std::env::remove_var("NINEROUTER_BROWSER_POOL");
        assert!(is_pool_enabled());
    }

    #[test]
    fn test_browser_pool_disabled_by_env_var() {
        for val in &["off", "0", "false"] {
            std::env::set_var("NINEROUTER_BROWSER_POOL", val);
            assert!(!is_pool_enabled(), "Pool should be disabled with {}", val);
        }
        std::env::remove_var("NINEROUTER_BROWSER_POOL");
    }

    #[test]
    fn test_default_user_agent_is_set() {
        assert!(!DEFAULT_USER_AGENT.is_empty());
        assert!(DEFAULT_USER_AGENT.contains("Chrome"));
    }

    #[test]
    fn test_acquire_context_options_default() {
        let opts = AcquireContextOptions::default();
        assert!(opts.headless);
        assert!(opts.cookie_string.is_none());
        assert!(opts.local_storage.is_none());
        assert!(opts.user_agent.is_none());
    }

    #[tokio::test]
    async fn test_acquire_context_fails_when_disabled() {
        std::env::set_var("NINEROUTER_BROWSER_POOL", "off");
        let opts = AcquireContextOptions::default();
        let result = acquire_browser_context("test-key", opts).await;
        assert!(result.is_err());
        std::env::remove_var("NINEROUTER_BROWSER_POOL");
    }

    #[tokio::test]
    async fn test_acquire_context_enabled_returns_error() {
        // For now, returns not-implemented error even when enabled
        // This will change once browser automation is added
        std::env::remove_var("NINEROUTER_BROWSER_POOL");
        let opts = AcquireContextOptions::default();
        let result = acquire_browser_context("test-key", opts).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_release_context_succeeds() {
        // Release should always succeed (it's a no-op for now)
        let result = release_browser_context("test-key").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_shutdown_pool_succeeds() {
        let result = shutdown_pool("test-shutdown").await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_get_browser_pool_status() {
        let status = get_browser_pool_status();
        assert_eq!(status.enabled, is_pool_enabled());
        assert_eq!(status.contexts, 0);
        assert!(!status.browser_running); // No browser running in stub
    }

    #[test]
    fn test_get_browser_pool_metrics() {
        let (status, metrics) = get_browser_pool_metrics();
        assert_eq!(status.enabled, is_pool_enabled());
        assert!(metrics.browser_launches >= 0);
    }
}
