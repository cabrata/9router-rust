//! MITM lifecycle + settings. Port of src/mitm/manager.js.
//!
//! The JS build spawns a standalone `server.js` child under sudo. Rust runs the
//! proxy in-process, so this module owns the task handle, the settings flags and
//! the sudo password cache instead of PIDs and PID files.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use nr_core::Result;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::task::JoinHandle;

use crate::mitm::{cert, dns, logger, server};

/// PORT 443 is required: intercepted IDE traffic goes to the real hostname.
pub const MITM_PORT: u16 = 443;

/// Settings key holding the enable flag. Same name as the JS build, so an
/// existing database keeps working.
pub const SETTING_ENABLED: &str = "mitmEnabled";
/// Settings key holding per-tool DNS state.
pub const SETTING_DNS_TOOLS: &str = "dnsToolEnabled";
/// Settings key for the router base URL override.
pub const SETTING_ROUTER_BASE: &str = "mitmRouterBaseUrl";

/// Auto-restart policy, mirroring the JS constants.
const MAX_RESTARTS: u32 = 5;
const RESTART_DELAYS_MS: [u64; 5] = [5_000, 10_000, 20_000, 30_000, 60_000];
const RESTART_RESET_MS: i64 = 60_000;

struct MitmState {
    task: Option<JoinHandle<()>>,
    /// Sudo password for hosts/cert writes. Never logged, never serialized.
    password: Option<String>,
    restarts: u32,
    last_start_ms: i64,
    stopping: bool,
}

static STATE: std::sync::LazyLock<Mutex<MitmState>> = std::sync::LazyLock::new(|| {
    Mutex::new(MitmState {
        task: None,
        password: None,
        restarts: 0,
        last_start_ms: 0,
        stopping: false,
    })
});

/// True once the proxy task has been spawned and hasn't finished.
pub fn is_running() -> bool {
    STATE.lock().task.as_ref().is_some_and(|t| !t.is_finished())
}

pub fn cached_password() -> Option<String> {
    STATE.lock().password.clone()
}

pub fn set_cached_password(pwd: Option<String>) {
    STATE.lock().password = pwd;
}

/// Read the `mitmEnabled` flag. DB errors read as disabled (fail-safe: don't start).
pub fn is_enabled() -> bool {
    let Ok(db) = nr_db::db() else { return false };
    nr_db::get_settings(&db)
        .ok()
        .and_then(|s| s.get(SETTING_ENABLED).and_then(|v| v.as_bool()))
        .unwrap_or(false)
}

fn write_setting(key: &str, value: Value) -> Result<()> {
    let db = nr_db::db()?;
    nr_db::update_settings(&db, json!({ key: value }))?;
    Ok(())
}

/// Persist the enabled flag. Best-effort during shutdown; explicit when called
/// from the API so a failed write is visible.
pub fn save_enabled(enabled: bool) -> Result<()> {
    write_setting(SETTING_ENABLED, Value::Bool(enabled))
}

fn save_dns_tool_state(tool: &str, enabled: bool) -> Result<()> {
    let db = nr_db::db()?;
    let current = nr_db::get_settings(&db)?;
    let mut map = current
        .get(SETTING_DNS_TOOLS)
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    map.insert(tool.to_string(), Value::Bool(enabled));
    nr_db::update_settings(&db, json!({ SETTING_DNS_TOOLS: Value::Object(map) }))?;
    Ok(())
}

fn dns_tool_state() -> serde_json::Map<String, Value> {
    let Ok(db) = nr_db::db() else {
        return Default::default();
    };
    nr_db::get_settings(&db)
        .ok()
        .and_then(|s| s.get(SETTING_DNS_TOOLS).and_then(|v| v.as_object()).cloned())
        .unwrap_or_default()
}

/// Start the proxy: ensure the CA exists, then spawn the TLS server task.
pub async fn start() -> Result<()> {
    {
        let mut st = STATE.lock();
        if st.task.as_ref().is_some_and(|t| !t.is_finished()) {
            return Err(nr_core::Error::other("MITM server is already running"));
        }
        st.stopping = false;
    }

    // Generate/load the CA before binding, so a broken CA is reported as such.
    let resolver = cert::resolver()?;

    let handle = tokio::spawn(async move {
        if let Err(e) = server::serve(MITM_PORT, resolver).await {
            logger::err(&format!("server stopped: {e}"));
        }
    });

    // Give the bind a moment so a port conflict surfaces as an error rather than
    // a silently dead task.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    if handle.is_finished() {
        return Err(nr_core::Error::other(format!(
            "MITM server failed to start. Port {MITM_PORT} may be in use (a local TLS server needs root)."
        )));
    }

    {
        let mut st = STATE.lock();
        st.task = Some(handle);
        st.last_start_ms = nr_core::json::now_ms();
    }
    save_enabled(true)?;
    logger::log(&format!("Server healthy on :{MITM_PORT}"));
    Ok(())
}

/// Stop the proxy: remove all hosts entries first, then drop the task.
///
/// Hosts entries go first because a stale `127.0.0.1` mapping for a tool host
/// without a listener breaks the IDE for every request, not just intercepted ones.
pub async fn stop() -> Result<()> {
    let (handle, password) = {
        let mut st = STATE.lock();
        st.stopping = true;
        st.restarts = 0;
        (st.task.take(), st.password.clone())
    };

    if let Some(handle) = handle {
        handle.abort();
        let _ = handle.await;
    }

    dns::remove_all_dns_entries(password.as_deref().unwrap_or("")).await;
    save_enabled(false)?;
    logger::log("Stopped");
    Ok(())
}

/// Full status payload for the settings API. Mirrors the JS `getMitmStatus`.
pub async fn status() -> Value {
    let cert_status = cert::install::status().await;
    json!({
        "running": is_running(),
        "certExists": cert_status.exists,
        "certTrusted": cert_status.trusted,
        "dnsStatus": dns::check_all_dns_status(),
        "hasCachedPassword": cached_password().is_some(),
        "needsSudoPassword": dns::is_sudo_password_required() && cached_password().is_none(),
        "port": MITM_PORT,
    })
}

/// Enable DNS interception for one tool (requires the proxy to be running).
pub async fn enable_tool_dns(tool: &str, password: Option<&str>) -> Result<()> {
    if !is_running() {
        return Err(nr_core::Error::other(
            "MITM server is not running. Start the server first.",
        ));
    }
    let pwd = password.map(str::to_string).or_else(cached_password).unwrap_or_default();
    dns::add_dns_entry(tool, &pwd).await?;
    save_dns_tool_state(tool, true)?;
    Ok(())
}

pub async fn disable_tool_dns(tool: &str, password: Option<&str>) -> Result<()> {
    let pwd = password.map(str::to_string).or_else(cached_password).unwrap_or_default();
    dns::remove_dns_entry(tool, &pwd).await?;
    save_dns_tool_state(tool, false)?;
    Ok(())
}

/// Install the Root CA into the system trust store without starting the proxy.
pub async fn trust_cert(password: Option<&str>) -> Result<()> {
    let path = cert::root_ca_cert_path();
    if !path.exists() {
        return Err(nr_core::Error::other(
            "Root CA not found. Start server first to generate it.",
        ));
    }
    let pwd = password.map(str::to_string).or_else(cached_password).unwrap_or_default();
    cert::install::install_cert(&path, &pwd).await
}

/// Re-apply DNS for tools that were enabled before a restart.
pub async fn restore_tool_dns() {
    let password = cached_password().unwrap_or_default();
    for (tool, enabled) in dns_tool_state() {
        if enabled.as_bool() != Some(true) {
            continue;
        }
        if let Err(e) = dns::add_dns_entry(&tool, &password).await {
            logger::err(&format!("DNS {tool}: restore failed — {e}"));
        }
    }
}

/// Auto-restart with exponential backoff after an unexpected exit.
///
/// Mirrors the JS policy: 5 attempts, the last delay repeated, and the counter
/// resets once the server has been up for a minute.
pub fn spawn_restart_watchdog() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let (needs_restart, attempt, stopping) = {
                let mut st = STATE.lock();
                let dead = st.task.as_ref().is_none_or(|t| t.is_finished());
                let stopping = st.stopping;
                if dead && !stopping {
                    if nr_core::json::now_ms() - st.last_start_ms >= RESTART_RESET_MS {
                        st.restarts = 0;
                    }
                    st.restarts += 1;
                }
                (dead && !stopping, st.restarts, stopping)
            };
            if stopping || !needs_restart || attempt == 0 {
                continue;
            }
            if attempt > MAX_RESTARTS {
                logger::err("Max restart attempts reached. Giving up.");
                continue;
            }
            if !is_enabled() {
                logger::log("MITM disabled, skipping restart");
                continue;
            }
            let idx = (attempt as usize - 1).min(RESTART_DELAYS_MS.len() - 1);
            let delay = RESTART_DELAYS_MS[idx];
            logger::log(&format!(
                "Restarting in {}s... ({attempt}/{MAX_RESTARTS})",
                delay / 1000
            ));
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;

            // Normalize the router base before the restart spawns.
            let resolver = match cert::resolver() {
                Ok(r) => r,
                Err(e) => {
                    logger::err(&format!("restart aborted, CA unavailable: {e}"));
                    continue;
                }
            };
            let handle = tokio::spawn(async move {
                if let Err(e) = server::serve(MITM_PORT, resolver).await {
                    logger::err(&format!("server stopped: {e}"));
                }
            });
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            if handle.is_finished() {
                logger::err(&format!("restart attempt {attempt}/{MAX_RESTARTS} failed"));
                continue;
            }
            {
                let mut st = STATE.lock();
                st.task = Some(handle);
                st.last_start_ms = nr_core::json::now_ms();
                st.restarts = 0;
            }
            logger::log("Restarted successfully");
        }
    });
}

/// Start the proxy on boot when the stored flag says it was enabled.
pub async fn start_if_enabled() {
    if !is_enabled() {
        return;
    }
    spawn_restart_watchdog();
    match start().await {
        Ok(()) => restore_tool_dns().await,
        Err(e) => logger::err(&format!("auto-start failed: {e}")),
    }
}

/// Share of the CA the settings UI needs (path only, never the key).
pub fn ca_path_string() -> String {
    cert::root_ca_cert_path().to_string_lossy().into_owned()
}

/// Registry of the Proxmox-style per-tool aliases the intercept path reads.
pub fn aliases_snapshot() -> Arc<Value> {
    Arc::new(crate::mitm::db_reader::get_all_mitm_aliases().unwrap_or_else(|| json!({})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_delays_match_js_policy() {
        assert_eq!(RESTART_DELAYS_MS, [5_000, 10_000, 20_000, 30_000, 60_000]);
        assert_eq!(MAX_RESTARTS, 5);
        assert_eq!(RESTART_RESET_MS, 60_000);
    }

    #[test]
    fn cached_password_never_leaks_through_status() {
        set_cached_password(Some("s3cret".into()));
        let status = futures::executor::block_on(status());
        let text = status.to_string();
        assert!(!text.contains("s3cret"), "status must not contain the password");
        assert_eq!(status["hasCachedPassword"], true);
        set_cached_password(None);
    }
}
