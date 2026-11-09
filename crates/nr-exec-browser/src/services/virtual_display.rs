//! Port of open-sse/services/virtualDisplay.js
//! Headed Chromium on a headless host (Xvfb wrapper for Linux).
//!
//! A headed browser needs an X display; Docker images and bare VPSs have none, so
//! a headed launch exits immediately. When DISPLAY/WAYLAND_DISPLAY is missing on
//! Linux this starts one private Xvfb per process and hands its DISPLAY to
//! Chromium. Hosts that already have a display are left untouched.

use parking_lot::Mutex;
use std::env;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const MISSING_DISPLAY_MESSAGE: &str =
    "Missing X server or $DISPLAY: a headed browser needs a display. \
Install Xvfb (apt-get install xvfb) or run with xvfb-run.";

const DEFAULT_TIMEOUT_MS: u64 = 5000;
const SCREEN: &str = "1280x1024x24";

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
        // Detail is appended to the same user-facing sentence the JS throws.
        let detail = match self {
            DisplayError::NoDisplay => return write!(f, "{MISSING_DISPLAY_MESSAGE}"),
            DisplayError::XvfbNotFound => "Xvfb not found".to_string(),
            DisplayError::XvfbStartTimeout => "Xvfb start timeout".to_string(),
            DisplayError::XvfbExited => "Xvfb exited".to_string(),
            DisplayError::Custom(msg) => msg.clone(),
        };
        write!(f, "{MISSING_DISPLAY_MESSAGE} ({detail})")
    }
}

impl std::error::Error for DisplayError {}

fn has_display_in(vars: &std::collections::HashMap<String, String>) -> bool {
    vars.contains_key("DISPLAY") || vars.contains_key("WAYLAND_DISPLAY")
}

pub fn has_display(env_vars: Option<&std::collections::HashMap<String, String>>) -> bool {
    if let Some(vars) = env_vars {
        has_display_in(vars)
    } else {
        env::var("DISPLAY").is_ok() || env::var("WAYLAND_DISPLAY").is_ok()
    }
}

struct Current {
    child: Child,
    display: String,
}

static CURRENT: Mutex<Option<Current>> = Mutex::new(None);
static STARTING: Mutex<()> = Mutex::new(());

fn x_lock_path(n: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/.X{n}-lock"))
}

fn x_socket_path(n: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/.X11-unix/X{n}"))
}

/// First display number whose lock file is absent (JS lets Xvfb pick one through
/// `-displayfd`; scanning the lock files avoids the extra pipe and is stable
/// enough because Xvfb itself refuses a display that is already in use).
fn first_free_display(start: u32) -> u32 {
    (start..start + 100).find(|n| !x_lock_path(*n).exists()).unwrap_or(start)
}

/// Ensure a display is available, starting Xvfb when the host has none.
///
/// Returns `Ok(None)` when this host already has a display (or is not Linux), so
/// callers hand `None` straight to Chromium's environment.
pub async fn ensure_virtual_display() -> Result<Option<VirtualDisplay>, DisplayError> {
    if std::env::consts::OS != "linux" || has_display(None) {
        return Ok(None);
    }

    {
        let mut current = CURRENT.lock();
        if let Some(running) = current.as_mut() {
            match running.child.try_wait() {
                Ok(None) => return Ok(Some(VirtualDisplay::new(running.display.clone()))),
                // Exited: drop it and start a fresh one.
                _ => *current = None,
            }
        }
    }

    // One starter at a time; a concurrent caller waits and then reuses the result.
    let _guard = STARTING.lock();
    if let Some(running) = CURRENT.lock().as_ref() {
        return Ok(Some(VirtualDisplay::new(running.display.clone())));
    }

    let started = start_xvfb(DEFAULT_TIMEOUT_MS).await?;
    let display = started.display.clone();
    *CURRENT.lock() = Some(started);
    Ok(Some(VirtualDisplay::new(display)))
}

async fn start_xvfb(timeout_ms: u64) -> Result<Current, DisplayError> {
    let n = first_free_display(99);
    let display = format!(":{n}");

    let mut child = Command::new("Xvfb")
        .arg(&display)
        .args(["-screen", "0", SCREEN, "-nolisten", "tcp"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                DisplayError::XvfbNotFound
            } else {
                DisplayError::Custom(e.to_string())
            }
        })?;

    // Xvfb creates the socket once the server is accepting connections; the lock
    // file alone appears earlier, so poll the socket and the child together.
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if x_socket_path(n).exists() {
            return Ok(Current { child, display });
        }
        match child.try_wait() {
            Ok(Some(_)) => return Err(DisplayError::XvfbExited),
            Ok(None) => {}
            Err(e) => return Err(DisplayError::Custom(e.to_string())),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            return Err(DisplayError::XvfbStartTimeout);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Stop the private display, if this process started one.
pub fn stop_virtual_display() {
    let mut current = CURRENT.lock();
    if let Some(mut running) = current.take() {
        let _ = running.child.kill();
        let _ = running.child.wait();
    }
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

    #[test]
    fn missing_display_message_names_the_fix() {
        let message = DisplayError::XvfbNotFound.to_string();
        assert!(message.starts_with(MISSING_DISPLAY_MESSAGE), "{message}");
        assert!(message.contains("Xvfb not found"), "{message}");
        assert!(MISSING_DISPLAY_MESSAGE.contains("apt-get install xvfb"));
    }

    #[test]
    fn free_display_skips_lock_files() {
        let n = first_free_display(700);
        assert!(!x_lock_path(n).exists());
        assert!(n >= 700);
    }

    /// Runs the real Xvfb when it is installed; skipped otherwise so CI without
    /// X11 packages stays green.
    #[tokio::test]
    async fn xvfb_starts_and_serves_a_socket() {
        if Command::new("Xvfb").arg("-help").stdout(Stdio::null()).stderr(Stdio::null()).status().is_err() {
            return;
        }
        let started = match start_xvfb(5000).await {
            Ok(started) => started,
            Err(DisplayError::XvfbNotFound) => return,
            Err(e) => panic!("Xvfb failed to start: {e}"),
        };
        let n: u32 = started.display.trim_start_matches(':').parse().expect("display number");
        assert!(x_socket_path(n).exists(), "socket for {}", started.display);
        let mut child = started.child;
        let _ = child.kill();
        let _ = child.wait();
    }
}
