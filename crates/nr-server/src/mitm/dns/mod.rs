//! Hosts-file interception. Port of src/mitm/dns/dnsConfig.js.
//!
//! The MITM proxy can only see traffic that reaches it. For each tool, the hosts
//! file is pointed at 127.0.0.1 so the IDE's HTTPS calls hit :443 locally.
//!
//! Privilege: writing /etc/hosts needs root on Linux (or sudo); on Windows it
//! needs Administrator. Everything here is fail-open on *reads* and explicit on
//! *writes*: a failed write surfaces as an error so the UI can say why.

use std::process::Stdio;

use nr_core::{Error, Result};
use serde_json::Value;

use crate::mitm::logger;

/// Per-tool DNS hosts — written to the hosts file as `127.0.0.1 <host>`.
/// Mirrors `src/shared/constants/mitmToolHosts.js` via the nr-data dump.
pub fn tool_hosts() -> Value {
    // `nr_data::export` already resolves `exports.<name>`.
    nr_data::export("src/shared/constants/mitmToolHosts", "TOOL_HOSTS")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}))
}

/// Hosts of one tool, or `None` for an unknown tool id.
pub fn hosts_for(tool: &str) -> Option<Vec<String>> {
    let hosts = tool_hosts();
    let list = hosts.get(tool)?.as_array()?;
    Some(
        list.iter()
            .filter_map(|h| h.as_str().map(str::to_string))
            .collect(),
    )
}

pub fn tool_names() -> Vec<String> {
    tool_hosts()
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

#[cfg(windows)]
fn hosts_file() -> std::path::PathBuf {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
    std::path::Path::new(&root).join("System32").join("drivers").join("etc").join("hosts")
}

#[cfg(not(windows))]
fn hosts_file() -> std::path::PathBuf {
    std::path::PathBuf::from("/etc/hosts")
}

/// `command -v sudo` — absent on minimal images (Alpine, distroless).
pub fn is_sudo_available() -> bool {
    if cfg!(windows) {
        return false;
    }
    std::process::Command::new("sh")
        .arg("-c")
        .arg("command -v sudo")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `sudo -n true` — true when sudo exists and needs no password.
pub fn can_run_sudo_without_password() -> bool {
    if cfg!(windows) || !is_sudo_available() {
        return true;
    }
    std::process::Command::new("sudo")
        .arg("-n")
        .arg("true")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn is_sudo_password_required() -> bool {
    !cfg!(windows) && is_sudo_available() && !can_run_sudo_without_password()
}

/// Run a shell command, feeding `password` to sudo on stdin when present.
///
/// Without sudo in PATH the command runs unprivileged in the same user's shell,
/// which matches JS behavior in containers.
pub async fn exec_with_password(command: &str, password: &str) -> Result<String> {
    let use_sudo = is_sudo_available();
    let mut cmd = if use_sudo {
        let mut c = tokio::process::Command::new("sudo");
        c.arg("-S").arg("sh").arg("-c").arg(command);
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(command);
        c
    };
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| Error::other(format!("spawn failed: {e}")))?;
    if use_sudo {
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            // A failed write only means sudo already exited (no password needed).
            let _ = stdin.write_all(format!("{password}\n").as_bytes()).await;
            let _ = stdin.shutdown().await;
        }
    }
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| Error::other(format!("command failed: {e}")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(Error::other(if stderr.is_empty() {
            format!("exit code {:?}", out.status.code())
        } else {
            stderr
        }))
    }
}

/// True when `host` already appears in the hosts file.
pub fn check_dns_entry(host: &str) -> bool {
    std::fs::read_to_string(hosts_file())
        .map(|c| c.contains(host))
        .unwrap_or(false)
}

/// Per-tool status: `{ "antigravity": true, ... }`. Unreadable hosts file → all false.
pub fn check_all_dns_status() -> serde_json::Map<String, Value> {
    let content = std::fs::read_to_string(hosts_file()).unwrap_or_default();
    let mut out = serde_json::Map::new();
    for tool in tool_names() {
        let active = hosts_for(&tool)
            .map(|hosts| !hosts.is_empty() && hosts.iter().all(|h| content.contains(h)))
            .unwrap_or(false);
        out.insert(tool, Value::Bool(active));
    }
    out
}

/// Rewrite the hosts file, preserving everything except our own entries.
///
/// Runs through sudo so the write is atomic from the caller's perspective and the
/// file keeps its ownership. Errors are returned, not logged away: the UI needs to
/// distinguish "wrong sudo password" from "already active".
async fn write_hosts(content: &str, password: &str) -> Result<()> {
    // Single-quote for the shell; embedded quotes are escaped as '\''.
    let escaped = content.replace('\'', "'\\''");
    let cmd = format!("printf '%s' '{escaped}' | tee {} > /dev/null", hosts_file().display());
    exec_with_password(&cmd, password).await.map(|_| ())
}

/// Flush the resolver cache so new entries take effect immediately.
async fn flush_dns(password: &str) -> Result<()> {
    if cfg!(windows) {
        return Ok(());
    }
    if cfg!(target_os = "macos") {
        exec_with_password("dscacheutil -flushcache && killall -HUP mDNSResponder", password).await?;
    } else {
        // `|| true`: resolvectl is missing on non-systemd hosts.
        exec_with_password("resolvectl flush-caches 2>/dev/null || true", password).await?;
    }
    Ok(())
}

/// Append `127.0.0.1 <host>` lines for a tool. No-op when already active.
pub async fn add_dns_entry(tool: &str, password: &str) -> Result<()> {
    let hosts = hosts_for(tool).ok_or_else(|| Error::other(format!("Unknown tool: {tool}")))?;
    let to_add: Vec<&String> = hosts.iter().filter(|h| !check_dns_entry(h)).collect();
    if to_add.is_empty() {
        logger::log(&format!("DNS {tool}: already active"));
        return Ok(());
    }

    let current = std::fs::read_to_string(hosts_file())
        .map_err(|e| Error::other(format!("read hosts: {e}")))?;
    let eol = if cfg!(windows) { "\r\n" } else { "\n" };
    let trimmed = current.trim_end();
    let block = to_add
        .iter()
        .map(|h| format!("127.0.0.1 {h}"))
        .collect::<Vec<_>>()
        .join(eol);
    let next = format!("{trimmed}{eol}{block}{eol}");

    write_hosts(&next, password)
        .await
        .map_err(|e| map_dns_error("add", &e))?;
    flush_dns(password).await.ok();
    logger::log(&format!(
        "DNS {tool}: added {}",
        to_add.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
    ));
    Ok(())
}

/// Remove a tool's entries from the hosts file.
pub async fn remove_dns_entry(tool: &str, password: &str) -> Result<()> {
    let hosts = hosts_for(tool).ok_or_else(|| Error::other(format!("Unknown tool: {tool}")))?;
    let present: Vec<&String> = hosts.iter().filter(|h| check_dns_entry(h)).collect();
    if present.is_empty() {
        logger::log(&format!("DNS {tool}: already inactive"));
        return Ok(());
    }

    let current = std::fs::read_to_string(hosts_file())
        .map_err(|e| Error::other(format!("read hosts: {e}")))?;
    let eol = if cfg!(windows) { "\r\n" } else { "\n" };
    let next = filter_hosts(&current, &present);
    write_hosts(&next, password)
        .await
        .map_err(|e| map_dns_error("remove", &e))?;
    flush_dns(password).await.ok();
    logger::log(&format!("DNS {tool}: removed entries"));
    Ok(())
}

/// Drop every line mentioning one of `hosts`, then normalize the trailing newline.
fn filter_hosts(content: &str, hosts: &[&String]) -> String {
    let eol = if cfg!(windows) { "\r\n" } else { "\n" };
    let kept: Vec<&str> = content
        .split('\n')
        .filter(|line| !hosts.iter().any(|h| line.contains(h.as_str())))
        .collect();
    format!("{}{eol}", kept.join(eol).trim_end())
}

fn map_dns_error(action: &str, e: &Error) -> Error {
    let msg = e.to_string();
    if msg.to_ascii_lowercase().contains("incorrect password") {
        Error::other("Wrong sudo password")
    } else {
        Error::other(format!("Failed to {action} DNS entry: {msg}"))
    }
}

/// Best-effort removal of every tool's entries (server shutdown path).
pub async fn remove_all_dns_entries(password: &str) {
    for tool in tool_names() {
        if let Err(e) = remove_dns_entry(&tool, password).await {
            logger::err(&format!("DNS {tool}: failed to remove — {e}"));
        }
    }
}

/// Synchronous removal for process shutdown, where async work isn't safe.
/// Assumes the caller already has write access to the hosts file.
pub fn remove_all_dns_entries_sync() {
    let Ok(content) = std::fs::read_to_string(hosts_file()) else {
        return;
    };
    let all: Vec<String> = tool_names()
        .iter()
        .filter_map(|t| hosts_for(t))
        .flatten()
        .collect();
    let refs: Vec<&String> = all.iter().collect();
    let next = filter_hosts(&content, &refs);
    if next == content {
        return;
    }
    if std::fs::write(hosts_file(), next).is_ok() {
        if cfg!(windows) {
            let _ = std::process::Command::new("ipconfig")
                .arg("/flushdns")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        } else if cfg!(target_os = "macos") {
            let _ = std::process::Command::new("sh")
                .arg("-c")
                .arg("dscacheutil -flushcache && killall -HUP mDNSResponder")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        } else {
            let _ = std::process::Command::new("sh")
                .arg("-c")
                .arg("resolvectl flush-caches 2>/dev/null || true")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_hosts_data_is_available() {
        let hosts = hosts_for("copilot").expect("copilot hosts");
        assert_eq!(hosts, vec!["api.individual.githubcopilot.com"]);
        assert!(hosts_for("nope").is_none());
    }

    #[test]
    fn filter_hosts_drops_only_our_lines() {
        let content = "127.0.0.1 localhost\n127.0.0.1 api2.cursor.sh\n10.0.0.1 other\n";
        let target = "api2.cursor.sh".to_string();
        let out = filter_hosts(content, &[&target]);
        assert!(!out.contains("api2.cursor.sh"));
        assert!(out.contains("localhost"));
        assert!(out.contains("10.0.0.1 other"));
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn filter_hosts_leaves_untouched_when_nothing_matches() {
        let content = "127.0.0.1 localhost\n";
        let target = "api2.cursor.sh".to_string();
        assert_eq!(filter_hosts(content, &[&target]), "127.0.0.1 localhost\n");
    }
}
