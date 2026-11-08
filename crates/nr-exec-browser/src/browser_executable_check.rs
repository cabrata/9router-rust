//! Browser executable detection and fallback logic.
//!
//! Port of open-sse/executors/browserExecutableCheck.js
//!
//! Distinguishes missing browser executables (config problem) from
//! transient upstream faults.

use std::path::Path;

/// Check if an error message indicates a missing browser executable.
/// This is a host/config problem, not a retryable error.
pub fn is_missing_browser_executable(message: &str) -> bool {
    if message.is_empty() {
        return false;
    }
    let lower = message.to_lowercase();
    lower.contains("executable doesn't exist")
        || lower.contains("executablenotfound")
        || lower.contains("playwright install")
        || (lower.contains("chromium") && lower.contains("download"))
}

/// Check if an error message indicates a missing X server for headed browser.
pub fn is_missing_display(message: &str) -> bool {
    if message.is_empty() {
        return false;
    }
    let lower = message.to_lowercase();
    lower.contains("xserver")
        || lower.contains("x server")
        || lower.contains("headed browser without")
        || lower.contains("$display")
}

/// Resolve system Chrome/Chromium executable path from environment or standard locations.
/// Returns the first path that exists, or None to let the browser use its bundled Chromium.
pub fn resolve_system_chrome_executable() -> Option<String> {
    let candidates = vec![
        std::env::var("NINEROUTER_BROWSER_BIN").ok(),
        std::env::var("CHROME_PATH").ok(),
        Some("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".to_string()),
        Some("/usr/bin/google-chrome".to_string()),
        Some("/usr/bin/google-chrome-stable".to_string()),
        Some("/usr/bin/chromium".to_string()),
        Some("/usr/bin/chromium-browser".to_string()),
    ];

    // Windows paths
    #[cfg(target_os = "windows")]
    let candidates = {
        let mut c = candidates;
        if let Ok(program_files) = std::env::var("PROGRAMFILES") {
            c.push(Some(format!(
                "{}\\Google\\Chrome\\Application\\chrome.exe",
                program_files
            )));
        }
        if let Ok(program_files) = std::env::var("PROGRAMFILES(X86)") {
            c.push(Some(format!(
                "{}\\Google\\Chrome\\Application\\chrome.exe",
                program_files
            )));
        }
        if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
            c.push(Some(format!(
                "{}\\Google\\Chrome\\Application\\chrome.exe",
                local_app_data
            )));
        }
        c
    };

    for candidate in candidates {
        if let Some(path) = candidate {
            let trimmed = path.trim();
            if !trimmed.is_empty() && Path::new(trimmed).exists() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_missing_browser_executable() {
        assert!(is_missing_browser_executable(
            "Executable doesn't exist at /path/to/chromium"
        ));
        assert!(is_missing_browser_executable("ExecutableNotFound"));
        assert!(is_missing_browser_executable(
            "playwright install --with-deps"
        ));
        assert!(is_missing_browser_executable(
            "Chromium download failed"
        ));
        assert!(!is_missing_browser_executable("Connection timeout"));
    }

    #[test]
    fn test_is_missing_display() {
        assert!(is_missing_display("No Xserver found"));
        assert!(is_missing_display("X server not available"));
        // JS isMissingDisplay() also returns false for this phrasing ("x-server").
        assert!(!is_missing_display(
            "A headed browser was requested without an X-Server"
        ));
        assert!(is_missing_display("$DISPLAY not set"));
        assert!(!is_missing_display("Network error"));
    }

    #[test]
    fn test_resolve_system_chrome_executable() {
        // On systems with Chrome installed, should find it
        // This test may pass or fail depending on the system
        let _result = resolve_system_chrome_executable();
        // We just verify it doesn't panic
    }
}
