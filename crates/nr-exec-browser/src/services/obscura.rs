//! Port of open-sse/services/obscura.js — shared Obscura browser engine.
//!
//! Obscura (https://github.com/h4ckf0r0day/obscura) is a lightweight Rust
//! headless browser (~30MB resident) that speaks the Chrome DevTools Protocol,
//! so the browser pool can drive it instead of holding a 150-400MB Chromium.
//!
//! Lifecycle: one `obscura serve` process is spawned lazily on first use and
//! shared for the process lifetime. Set `OBSCURA_CDP_ENDPOINT` to point at an
//! already-running Obscura (that process is then not owned here). The module is
//! disabled when `NINEROUTER_BROWSER_POOL=off`.
//!
//! shortcut: `connectObscuraBrowser` returns a CDP endpoint rather than a
//! Playwright browser handle, because this port talks CDP directly instead of
//! embedding playwright-core. Upgrade when a CDP client (chromiumoxide) is wired
//! into the pool.

use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use lazy_static::lazy_static;

const READY_TIMEOUT: Duration = Duration::from_millis(30_000);
const PROBE_INTERVAL: Duration = Duration::from_millis(250);
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// The shared Obscura server: a CDP endpoint plus the process we own (if any).
pub struct ObscuraServer {
    pub endpoint: String,
    child: Option<Child>,
}

impl ObscuraServer {
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

struct SharedState {
    endpoint: Option<String>,
    // The child must live as long as the endpoint; it is never awaited.
    child: Option<Child>,
}

lazy_static! {
    static ref SHARED: Mutex<SharedState> = Mutex::new(SharedState { endpoint: None, child: None });
}

/// `isObscuraUsable` — the module is off when the browser pool is off.
pub fn is_obscura_usable() -> bool {
    match std::env::var("NINEROUTER_BROWSER_POOL") {
        Ok(flag) => flag != "off" && flag != "0" && flag != "false",
        Err(_) => true,
    }
}

/// Find the `obscura` binary: `OBSCURA_BIN`, else the first executable match on PATH.
fn obscura_binary_path() -> Option<String> {
    if let Ok(bin) = std::env::var("OBSCURA_BIN") {
        let bin = bin.trim();
        if !bin.is_empty() {
            return Some(bin.to_string());
        }
    }
    let path_var = std::env::var("PATH").ok()?;
    for dir in path_var.split(':') {
        if dir.is_empty() {
            continue;
        }
        let candidate = std::path::Path::new(dir).join("obscura");
        if is_executable(&candidate) {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    path.is_file()
}

/// Allocate an ephemeral local port by binding to :0 and reading it back.
fn find_free_port() -> Option<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
    listener.local_addr().ok().map(|a| a.port())
}

/// Probe `{endpoint}/json/version` until it answers or the deadline passes.
///
/// Obscura's HTTP server answers the CDP info route; a bare GET to `/` never
/// completes a response, so the version path is the readiness signal.
async fn wait_for_cdp_endpoint(endpoint: &str, timeout: Duration) -> bool {
    let base = endpoint.trim_end_matches('/').replace("ws://", "http://");
    let probe = format!("{base}/json/version");
    let client = match reqwest::Client::builder().timeout(PROBE_TIMEOUT).build() {
        Ok(c) => c,
        Err(_) => return false,
    };
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(res) = client.get(&probe).send().await {
            if res.status().is_success() {
                return true;
            }
        }
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
    false
}

/// `OBSCURA_CDP_ENDPOINT`: an externally managed server we do not own.
fn external_endpoint() -> Option<String> {
    let endpoint = std::env::var("OBSCURA_CDP_ENDPOINT").ok()?;
    let endpoint = endpoint.trim().to_string();
    if endpoint.is_empty() {
        return None;
    }
    Some(endpoint)
}

/// Ensure the shared Obscura server is up; returns its endpoint or `None`.
pub async fn ensure_obscura_server() -> Option<String> {
    if !is_obscura_usable() {
        return None;
    }
    if let Some(endpoint) = SHARED.lock().ok().and_then(|s| s.endpoint.clone()) {
        return Some(endpoint);
    }

    // An externally managed server: record it and do not own a child process.
    if let Some(endpoint) = external_endpoint() {
        if let Ok(mut shared) = SHARED.lock() {
            shared.endpoint = Some(endpoint.clone());
        }
        return Some(endpoint);
    }

    let bin = obscura_binary_path()?;
    let port = match std::env::var("OBSCURA_PORT").ok().and_then(|p| p.parse::<u16>().ok()) {
        Some(p) if p > 0 => p,
        _ => find_free_port()?,
    };

    let mut child = Command::new(&bin)
        .arg("serve")
        .arg("--port")
        .arg(port.to_string())
        .arg("--host")
        .arg("127.0.0.1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null()) // obscura logs verbosely — swallow
        .spawn()
        .ok()?;

    let endpoint = format!("http://127.0.0.1:{port}");

    // A bad binary must not hold the readiness wait for the full timeout: bail
    // as soon as the child exits.
    let ready = tokio::select! {
        ok = wait_for_cdp_endpoint(&endpoint, READY_TIMEOUT) => ok,
        _ = wait_for_exit(&mut child) => false,
    };

    if !ready {
        let _ = child.kill();
        return None;
    }

    if let Ok(mut shared) = SHARED.lock() {
        shared.endpoint = Some(endpoint.clone());
        shared.child = Some(child);
    }
    Some(endpoint)
}

/// Resolve when the child process exits (used to fail the readiness wait fast).
async fn wait_for_exit(child: &mut Child) {
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => tokio::time::sleep(PROBE_INTERVAL).await,
        }
    }
}

/// Connect to the shared Obscura server.
///
/// Returns `None` when Obscura is disabled, not installed, or could not start,
/// so callers fall back to their previous Chromium strategy.
///
/// shortcut: returns the CDP endpoint instead of a browser handle (this port
/// has no playwright-core). Wire it into the CDP client in `crate::cdp` when
/// that client grows a real transport.
pub async fn connect_obscura_browser() -> Option<ObscuraServer> {
    if !is_obscura_usable() {
        return None;
    }
    let endpoint = ensure_obscura_server().await?;
    // An externally managed server is not owned here — no child to hand out.
    let child = if external_endpoint().as_deref() == Some(endpoint.as_str()) {
        None
    } else {
        SHARED.lock().ok().and_then(|mut s| s.child.take())
    };
    Some(ObscuraServer { endpoint, child })
}

/// Terminate the shared `obscura serve` process (if this module owns it).
pub fn kill_shared_obscura_server() {
    if let Ok(mut shared) = SHARED.lock() {
        if let Some(mut child) = shared.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        shared.endpoint = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_obscura_usable_by_default() {
        std::env::remove_var("NINEROUTER_BROWSER_POOL");
        assert!(is_obscura_usable());
    }

    #[test]
    fn test_obscura_disabled_flag() {
        std::env::set_var("NINEROUTER_BROWSER_POOL", "off");
        assert!(!is_obscura_usable());
        std::env::remove_var("NINEROUTER_BROWSER_POOL");
    }

    #[tokio::test]
    async fn test_ensure_returns_none_when_disabled() {
        std::env::set_var("NINEROUTER_BROWSER_POOL", "off");
        assert!(ensure_obscura_server().await.is_none());
        std::env::remove_var("NINEROUTER_BROWSER_POOL");
    }

    #[test]
    fn test_find_free_port() {
        let port = find_free_port().expect("a free port");
        assert!(port > 0);
    }
}
