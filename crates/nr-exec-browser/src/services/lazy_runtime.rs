//! Port of open-sse/services/lazyRuntime.js
//! On-demand dependency loading (for Node deps, we stub with error messages in Rust)

use std::env;

/// Placeholder for runtime dependency loading.
/// In Node, this loads playwright-core on demand.
/// In Rust, we provide a structured error message.
pub async fn load_runtime_dep(package: &str) -> Result<String, String> {
    match package {
        "playwright-core" => {
            // In Rust, browser automation is handled differently
            // This would be replaced with a real CDP client in production
            Err(format!(
                "{} is required but not available. Set up a CDP endpoint or use --browser-pool off",
                package
            ))
        }
        "wreq-js" => {
            Err(format!(
                "{} (HTTP request library) is not available in Rust build",
                package
            ))
        }
        _ => Err(format!("Unknown runtime dependency: {}", package)),
    }
}

/// Get writable data directory for runtime installations
pub fn get_runtime_dir() -> String {
    // In JS, this is ~/.9router/runtime
    // In Rust, we use a similar pattern from nr_core::paths
    env::var("DATA_DIR")
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .map(|h| h.join(".9router").join("runtime").to_string_lossy().to_string())
                .unwrap_or_else(|| "/tmp/9router-runtime".to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_load_runtime_dep_playwright() {
        let result = load_runtime_dep("playwright-core").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_load_runtime_dep_unknown() {
        let result = load_runtime_dep("unknown-package").await;
        assert!(result.is_err());
    }
}
