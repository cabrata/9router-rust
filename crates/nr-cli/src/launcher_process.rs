//! Fast previous-instance detection for the launcher.
//!
//! The launcher records its own PID and its server's PID in <dataDir>/launcher.pid.
//! The next launch stops exactly those processes instead of scanning every process
//! on the machine (which costs ~0.5s per call on Windows), and only falls back to the
//! full scan when the port is still occupied.

use anyhow::Result;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub const PID_FILE_NAME: &str = "launcher.pid";

/// Record of a launcher instance: its PID, server PID, port, and executable name.
#[derive(Debug, Clone)]
pub struct PidRecord {
    pub cli_pid: u32,
    pub server_pid: Option<u32>,
    pub port: Option<u16>,
    pub exe: String,
}

impl PidRecord {
    /// Create a new PID record for this launcher instance.
    pub fn new(cli_pid: u32, server_pid: Option<u32>, port: Option<u16>, exe: String) -> Self {
        Self {
            cli_pid,
            server_pid,
            port,
            exe,
        }
    }

    /// Serialize to JSON.
    fn to_json(&self) -> Value {
        json!({
            "cliPid": self.cli_pid,
            "serverPid": self.server_pid,
            "port": self.port,
            "exe": self.exe
        })
    }

    /// Deserialize from JSON value.
    fn from_value(v: &Value) -> Option<Self> {
        let obj = v.as_object()?;
        let cli_pid = obj.get("cliPid")?.as_u64()? as u32;
        if cli_pid == 0 {
            return None;
        }
        let exe = obj.get("exe")?.as_str()?.to_string();
        if exe.is_empty() {
            return None;
        }
        let server_pid = obj.get("serverPid")?.as_u64().map(|p| p as u32).filter(|&p| p > 0);
        let port = obj.get("port")?.as_u64().map(|p| p as u16);

        Some(Self {
            cli_pid,
            server_pid,
            port,
            exe,
        })
    }
}

/// Get the path to the launcher PID file in the given data directory.
pub fn get_pid_file_path(data_dir: &Path) -> PathBuf {
    data_dir.join(PID_FILE_NAME)
}

/// Write a PID record to file atomically (write to temp, then rename).
pub fn write_pid_record(file_path: &Path, record: &PidRecord) -> Result<()> {
    let dir = file_path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;

    let tmp_path = file_path.with_extension(&format!("{}.tmp", std::process::id()));
    let json_str = serde_json::to_string(&record.to_json())?;
    fs::write(&tmp_path, json_str)?;
    fs::rename(&tmp_path, file_path)?;
    Ok(())
}

/// Read a PID record from file.
pub fn read_pid_record(file_path: &Path) -> Result<Option<PidRecord>> {
    match fs::read_to_string(file_path) {
        Ok(content) => {
            match serde_json::from_str::<Value>(&content) {
                Ok(v) => Ok(PidRecord::from_value(&v)),
                Err(_) => Ok(None),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Remove the PID record file if it was created by the given owner PID.
pub fn remove_pid_record_if_owner(file_path: &Path, owner_pid: u32) -> Result<()> {
    if let Ok(Some(record)) = read_pid_record(file_path) {
        if record.cli_pid == owner_pid {
            let _ = fs::remove_file(file_path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_pid_record_round_trip() {
        let record = PidRecord::new(1234, Some(5678), Some(20128), "9router".to_string());
        let json = record.to_json();
        let recovered = PidRecord::from_value(&json).expect("should deserialize");
        assert_eq!(recovered.cli_pid, 1234);
        assert_eq!(recovered.server_pid, Some(5678));
        assert_eq!(recovered.port, Some(20128));
        assert_eq!(recovered.exe, "9router");
    }

    #[test]
    fn test_pid_record_no_server_pid() {
        let record = PidRecord::new(1234, None, Some(20128), "9router".to_string());
        let json = record.to_json();
        let recovered = PidRecord::from_value(&json).expect("should deserialize");
        assert_eq!(recovered.server_pid, None);
    }

    #[test]
    fn test_write_read_pid_file() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let file_path = temp_dir.path().join("launcher.pid");

        let record = PidRecord::new(1234, Some(5678), Some(20128), "9router".to_string());
        write_pid_record(&file_path, &record)?;

        let read_back = read_pid_record(&file_path)?;
        assert!(read_back.is_some());
        let recovered = read_back.unwrap();
        assert_eq!(recovered.cli_pid, 1234);
        Ok(())
    }

    #[test]
    fn test_remove_pid_record_if_owner() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let file_path = temp_dir.path().join("launcher.pid");

        let record = PidRecord::new(1234, Some(5678), Some(20128), "9router".to_string());
        write_pid_record(&file_path, &record)?;
        assert!(file_path.exists());

        remove_pid_record_if_owner(&file_path, 1234)?;
        assert!(!file_path.exists());
        Ok(())
    }

    #[test]
    fn test_remove_pid_record_wrong_owner() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let file_path = temp_dir.path().join("launcher.pid");

        let record = PidRecord::new(1234, Some(5678), Some(20128), "9router".to_string());
        write_pid_record(&file_path, &record)?;
        assert!(file_path.exists());

        remove_pid_record_if_owner(&file_path, 9999)?; // Different owner
        assert!(file_path.exists()); // Should not remove
        Ok(())
    }
}
