//! Port of open-sse/shared/machineId.js — Generate consistent machine ID.

use std::sync::LazyLock;
use sha2::{Sha256, Digest};

static MACHINE_ID_CACHE: LazyLock<String> = LazyLock::new(|| {
    load_raw_machine_id().unwrap_or_else(|_| uuid::Uuid::new_v4().to_string())
});

/// Load raw machine ID using system info or generate a UUID fallback.
fn load_raw_machine_id() -> Result<String, Box<dyn std::error::Error>> {
    // Try to get machine ID from /etc/machine-id (Linux)
    if let Ok(id) = std::fs::read_to_string("/etc/machine-id") {
        return Ok(id.trim().to_string());
    }

    // Try to get from /var/db/dbus/machine-id (macOS/BSD)
    if let Ok(id) = std::fs::read_to_string("/var/db/dbus/machine-id") {
        return Ok(id.trim().to_string());
    }

    // Fallback: generate a UUID
    Ok(uuid::Uuid::new_v4().to_string())
}

/// Get a consistent 16-character machine ID based on the raw machine ID and salt.
pub fn get_consistent_machine_id(salt: &str) -> String {
    let raw_id = MACHINE_ID_CACHE.as_str();
    let salted = format!("{}{}", raw_id, salt);
    let mut hasher = Sha256::new();
    hasher.update(salted.as_bytes());
    let digest = hasher.finalize();
    let hex = format!("{:x}", digest);
    hex[0..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_consistent_machine_id() {
        let id = get_consistent_machine_id("test-salt");
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_get_consistent_machine_id_deterministic() {
        let id1 = get_consistent_machine_id("test-salt");
        let id2 = get_consistent_machine_id("test-salt");
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_get_consistent_machine_id_different_salts() {
        let id1 = get_consistent_machine_id("salt1");
        let id2 = get_consistent_machine_id("salt2");
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_get_consistent_machine_id_default_salt() {
        let id = get_consistent_machine_id("endpoint-proxy-salt");
        assert_eq!(id.len(), 16);
    }
}
