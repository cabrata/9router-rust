//! Process control: spawning, killing, and tracking server processes.
//!
//! Includes logic to safely kill processes by PID, handling platform differences
//! and guarding against PID reuse by matching executable names.
//!
//! ## Platform-specific implementation
//!
//! - **Unix (Linux, macOS)**: Uses libc `kill(pid, 0)` to check if process is alive,
//!   `kill(pid, SIGKILL)` to forcefully terminate.
//! - **Windows**: Uses `tasklist` to check if process exists, `taskkill /F /T /PID` to terminate.
//!
//! ## PID reuse guard
//!
//! Since PIDs are reused after process death, we verify the executable name matches
//! before killing to prevent terminating unrelated processes that happened to reuse
//! the same PID (e.g., editor/grep that starts after a killed 9router process).
//! This is critical since launcher.pid is stale-safe but not foolproof.

use anyhow::Result;
use std::path::Path;
use std::process::Command;

#[cfg(unix)]
use libc::{kill, SIGKILL};

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

use crate::launcher_process::PidRecord;

/// Check if a process with the given PID is alive.
pub fn is_pid_alive(pid: u32) -> bool {
    // pid 0 means "the whole process group" to kill(2), not a real process.
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        // On Unix, kill with signal 0 returns 0 if process exists, ESRCH if not
        unsafe { kill(pid as libc::pid_t, 0) == 0 }
    }

    #[cfg(windows)]
    {
        // Windows: use tasklist to check if PID exists
        match Command::new("tasklist")
            .args(["/FI", &format!("PID eq {}", pid)])
            .output()
        {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                stdout.contains(&pid.to_string())
            }
            Err(_) => false,
        }
    }
}

/// Normalize an executable name by removing .exe suffix and lowercasing.
fn normalize_exe_name(name: &str) -> String {
    let lower = name.to_lowercase();
    let trimmed = lower.trim();
    trimmed
        .strip_suffix(".exe")
        .unwrap_or(trimmed)
        .to_string()
}

/// Get the executable name for a process by PID.
#[cfg(unix)]
fn get_process_exe_name(pid: u32) -> Result<String> {
    use std::fs;

    // Try to read /proc/{pid}/exe (Linux)
    if let Ok(exe_path) = fs::read_link(format!("/proc/{}/exe", pid)) {
        return Ok(exe_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string());
    }

    // Fallback to ps command
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()?;

    Ok(String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_string())
}

#[cfg(windows)]
fn get_process_exe_name(pid: u32) -> Result<String> {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {}", pid), "/FO", "CSV", "/NH"])
        .output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    // CSV format: "name","pid"
    if let Some(line) = stdout.lines().next() {
        if let Some(start) = line.find('"') {
            if let Some(end) = line[start + 1..].find('"') {
                return Ok(line[start + 1..start + 1 + end].to_string());
            }
        }
    }
    Ok(String::new())
}

/// Guard against PID reuse: check if the process matches the expected executable name.
fn process_matches_exe(pid: u32, expected_exe: &str) -> bool {
    let expected = normalize_exe_name(expected_exe);
    if expected.is_empty() {
        return false;
    }

    match get_process_exe_name(pid) {
        Ok(actual) => {
            let actual_base = Path::new(&actual)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            normalize_exe_name(actual_base) == expected
        }
        Err(_) => false,
    }
}

/// Kill a process by PID.
pub fn kill_by_pid(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        unsafe {
            if kill(pid as libc::pid_t, SIGKILL) != 0 {
                return Err(anyhow::anyhow!("failed to kill process {}", pid));
            }
        }
        Ok(())
    }

    #[cfg(windows)]
    {
        let status = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .creation_flags(0x08000000) // CREATE_NO_WINDOW
            .status()?;

        if status.success() {
            Ok(())
        } else {
            Err(anyhow::anyhow!("taskkill failed for PID {}", pid))
        }
    }
}

/// Stop the processes named in a launcher record.
/// Returns the PIDs it actually killed (filtered for safety).
pub fn stop_recorded_instance(record: &PidRecord, self_pid: u32) -> Vec<u32> {
    let mut killed = Vec::new();

    // Try server PID first so port is released even if CLI kill fails
    if let Some(server_pid) = record.server_pid {
        if server_pid != self_pid
            && is_pid_alive(server_pid)
            && process_matches_exe(server_pid, &record.exe)
        {
            if kill_by_pid(server_pid).is_ok() {
                killed.push(server_pid);
            }
        }
    }

    // Then try CLI PID
    if record.cli_pid != self_pid
        && is_pid_alive(record.cli_pid)
        && process_matches_exe(record.cli_pid, &record.exe)
    {
        if kill_by_pid(record.cli_pid).is_ok() {
            killed.push(record.cli_pid);
        }
    }

    killed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_exe_name() {
        assert_eq!(normalize_exe_name("9router.exe"), "9router");
        assert_eq!(normalize_exe_name("9Router.EXE"), "9router");
        assert_eq!(normalize_exe_name("9router"), "9router");
    }
}
