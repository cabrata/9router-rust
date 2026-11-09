//! Port of open-sse/services/virtualDisplay.js
//! Headed Chromium on headless host (Xvfb wrapper for Linux)

use lazy_static::lazy_static;
use parking_lot::Mutex;
use std::env;
use std::process::{Child, Command};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct VirtualDisplay {
    display: String,
}

impl VirtualDisplay {
    pub fn new(display: String) -> Self {
        Self { display }
    }

    pub fn display(&self) -> &str {
        &self.display
    }
}

#[derive(Debug)]
pub enum DisplayError {
    NoDisplay,
    XvfbNotFound,
    XvfbStartTimeout,
    XvfbExited,
    Custom(String),
}

impl std::fmt::Display for DisplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            DisplayError::NoDisplay => write!(f, "Missing X server or $DISPLAY"),
            DisplayError::XvfbNotFound => write!(f, "Xvfb not found"),
            DisplayError::XvfbStartTimeout => write!(f, "Xvfb start timeout"),
            DisplayError::XvfbExited => write!(f, "Xvfb exited prematurely"),
            DisplayError::Custom(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for DisplayError {}

pub fn has_display(env_vars: Option<&std::collections::HashMap<String, String>>) -> bool {
    if let Some(vars) = env_vars {
        vars.contains_key("DISPLAY") || vars.contains_key("WAYLAND_DISPLAY")
    } else {
        env::var("DISPLAY").is_ok() || env::var("WAYLAND_DISPLAY").is_ok()
    }
}

/// Ensure a virtual display is available (or use existing one)
pub async fn ensure_virtual_display() -> Result<Option<VirtualDisplay>, DisplayError> {
    let platform = std::env::consts::OS;

    // Only Linux needs this
    if platform != "linux" {
        return Ok(None);
    }

    // Already have a display
    if has_display(None) {
        return Ok(None);
    }

    // Try to start Xvfb
    start_xvfb().await
}

/// Stop the virtual display
pub fn stop_virtual_display() {
    // Cleanup is handled by process drop
}

async fn start_xvfb() -> Result<Option<VirtualDisplay>, DisplayError> {
    // This would need to spawn Xvfb in real implementation
    // For now, return None since we don't have Xvfb integration ready
    // TODO: implement actual Xvfb spawning
    Err(DisplayError::XvfbNotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_has_display_without_display() {
        let vars = HashMap::new();
        assert!(!has_display(Some(&vars)));
    }

    #[test]
    fn test_has_display_with_display() {
        let mut vars = HashMap::new();
        vars.insert("DISPLAY".to_string(), ":0".to_string());
        assert!(has_display(Some(&vars)));
    }

    #[test]
    fn test_has_display_with_wayland() {
        let mut vars = HashMap::new();
        vars.insert("WAYLAND_DISPLAY".to_string(), "wayland-0".to_string());
        assert!(has_display(Some(&vars)));
    }
}
