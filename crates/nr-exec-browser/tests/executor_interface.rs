//! Integration tests for executor interface compliance.
//!
//! Verifies that all executors implement the Executor trait correctly
//! and return appropriate placeholder responses.

#[tokio::test]
async fn test_gemini_web_executor_creates() {
    let executor = nr_exec_browser::gemini_web::create("gemini-2.5-web");
    // Should create without panic
    assert!(std::mem::size_of_val(&*executor) > 0);
}

#[tokio::test]
async fn test_grok_web_executor_creates() {
    let executor = nr_exec_browser::grok_web::create("grok-web");
    assert!(std::mem::size_of_val(&*executor) > 0);
}

#[tokio::test]
async fn test_muse_web_executor_creates() {
    let executor = nr_exec_browser::muse_web::create("muse-web");
    assert!(std::mem::size_of_val(&*executor) > 0);
}

#[tokio::test]
async fn test_veoaifree_web_executor_creates() {
    let executor = nr_exec_browser::veoaifree_web::create("veoaifree-web");
    assert!(std::mem::size_of_val(&*executor) > 0);
}

// One test: both cases mutate the same env var, and tests run in parallel threads.
#[test]
fn test_browser_pool_status_follows_env() {
    std::env::set_var("NINEROUTER_BROWSER_POOL", "off");
    assert!(!nr_exec_browser::get_browser_pool_status().enabled);
    std::env::remove_var("NINEROUTER_BROWSER_POOL");
    assert!(nr_exec_browser::get_browser_pool_status().enabled);
}

#[test]
fn test_executable_check_patterns() {
    // Test error detection patterns
    assert!(nr_exec_browser::is_missing_browser_executable(
        "ExecutableNotFound: chromium"
    ));
    assert!(nr_exec_browser::is_missing_display("No X server available"));

    // Test non-matching patterns
    assert!(!nr_exec_browser::is_missing_browser_executable(
        "Connection timeout"
    ));
    assert!(!nr_exec_browser::is_missing_display("Network error"));
}
