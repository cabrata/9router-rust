//! Port of open-sse/utils/cursorChecksum.js
//! Cursor checksum (Jyh cipher) header generation

use sha2::{Sha256, Digest};
use uuid::{uuid, Uuid};

/// Generate SHA-256 hash (64-char hex)
pub fn generate_hashed64_hex(input: &str, salt: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{}{}", input, salt));
    format!("{:x}", hasher.finalize())
}

/// Generate session ID using UUID v5 with DNS namespace
pub fn generate_session_id(auth_token: &str) -> String {
    let dns_ns = uuid!("6ba7b810-9dad-11d1-80b4-00c04fd430c8");
    Uuid::new_v5(&dns_ns, auth_token.as_bytes()).to_string()
}

/// Generate cursor checksum (Jyh cipher)
/// Algorithm:
/// 1. Encode timestamp (current time in microseconds, 6 bytes big-endian)
/// 2. XOR each byte with key (starting 165)
/// 3. Update key: key = (key + byte) & 0xFF
/// 4. URL-safe base64 encode (no padding)
/// 5. Format: {base64_encoded}{machineId}
pub fn generate_cursor_checksum(machine_id: &str) -> String {
    // Timestamp in microseconds (Date.now() / 1000000 in JS, which is in milliseconds / 1000000)
    let now_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;

    // Create 6-byte big-endian array
    let mut byte_array = [
        ((now_us >> 40) & 0xFF) as u8,
        ((now_us >> 32) & 0xFF) as u8,
        ((now_us >> 24) & 0xFF) as u8,
        ((now_us >> 16) & 0xFF) as u8,
        ((now_us >> 8) & 0xFF) as u8,
        (now_us & 0xFF) as u8,
    ];

    // Jyh cipher obfuscation
    let mut t = 165u8;
    for i in 0..byte_array.len() {
        byte_array[i] = ((byte_array[i] ^ t).wrapping_add(i as u8)) & 0xFF;
        t = byte_array[i];
    }

    // URL-safe base64 encode (without padding)
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut encoded = String::new();

    for chunk in byte_array.chunks(3) {
        let a = chunk[0];
        let b = if chunk.len() > 1 { chunk[1] } else { 0 };
        let c = if chunk.len() > 2 { chunk[2] } else { 0 };

        encoded.push(ALPHABET[(a >> 2) as usize] as char);
        encoded.push(ALPHABET[(((a & 3) << 4) | (b >> 4)) as usize] as char);

        if chunk.len() > 1 {
            encoded.push(ALPHABET[(((b & 15) << 2) | (c >> 6)) as usize] as char);
        }
        if chunk.len() > 2 {
            encoded.push(ALPHABET[(c & 63) as usize] as char);
        }
    }

    format!("{}{}", encoded, machine_id)
}

/// Build all Cursor API headers
pub fn build_cursor_headers(
    access_token: &str,
    machine_id: Option<&str>,
    ghost_mode: bool,
) -> indexmap::IndexMap<String, String> {
    // Clean token if it has prefix
    let clean_token = if access_token.contains("::") {
        access_token.split("::").nth(1).unwrap_or(access_token)
    } else {
        access_token
    };

    // Generate machine ID if not provided
    let effective_machine_id = if let Some(mid) = machine_id {
        mid.to_string()
    } else {
        generate_hashed64_hex(clean_token, "machineId")
    };

    // Generate derived values
    let session_id = generate_session_id(clean_token);
    let client_key = generate_hashed64_hex(clean_token, "");
    let checksum = generate_cursor_checksum(&effective_machine_id);

    // Detect OS and architecture
    #[cfg(target_os = "windows")]
    let os = "windows";
    #[cfg(target_os = "macos")]
    let os = "macos";
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let os = "linux";

    #[cfg(target_arch = "aarch64")]
    let arch = "aarch64";
    #[cfg(not(target_arch = "aarch64"))]
    let arch = "x64";

    // Get timezone (simplified: use UTC as fallback)
    let timezone = "UTC";

    // Generate trace and config IDs
    let trace_id = Uuid::new_v4();
    let config_version = Uuid::new_v4();
    let request_id = Uuid::new_v4();

    let mut headers = indexmap::IndexMap::new();
    headers.insert("authorization".to_string(), format!("Bearer {}", clean_token));
    headers.insert("connect-accept-encoding".to_string(), "gzip".to_string());
    headers.insert("connect-protocol-version".to_string(), "1".to_string());
    headers.insert("content-type".to_string(), "application/connect+proto".to_string());
    headers.insert("user-agent".to_string(), "connect-es/1.6.1".to_string());
    headers.insert("x-amzn-trace-id".to_string(), format!("Root={}", trace_id));
    headers.insert("x-client-key".to_string(), client_key);
    headers.insert("x-cursor-checksum".to_string(), checksum);
    headers.insert("x-cursor-client-version".to_string(), "3.12.17".to_string());
    headers.insert("x-cursor-client-commit".to_string(), "0fb762053c34788bb7760d5673f8a6d4c8589d50".to_string());
    headers.insert("x-cursor-client-type".to_string(), "ide".to_string());
    headers.insert("x-cursor-client-os".to_string(), os.to_string());
    headers.insert("x-cursor-client-arch".to_string(), arch.to_string());
    headers.insert("x-cursor-client-device-type".to_string(), "desktop".to_string());
    headers.insert("x-cursor-config-version".to_string(), config_version.to_string());
    headers.insert("x-cursor-timezone".to_string(), timezone.to_string());
    headers.insert("x-ghost-mode".to_string(), if ghost_mode { "true" } else { "false" }.to_string());
    headers.insert("x-request-id".to_string(), request_id.to_string());
    headers.insert("x-session-id".to_string(), session_id);

    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_hashed64_hex() {
        let result = generate_hashed64_hex("test", "");
        assert_eq!(result.len(), 64);
        assert!(result.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_generate_session_id() {
        let result = generate_session_id("test-token");
        assert!(result.len() > 0);
        // Should be a valid UUID format
        assert!(result.contains('-'));
    }

    #[test]
    fn test_generate_cursor_checksum() {
        let checksum = generate_cursor_checksum("test-machine-id");
        assert!(checksum.ends_with("test-machine-id"));
        // Should have base64 prefix before machine ID
        assert!(checksum.len() > "test-machine-id".len());
    }

    #[test]
    fn test_build_cursor_headers() {
        let headers = build_cursor_headers("test-token", Some("my-machine-id"), true);
        assert!(headers.contains_key("authorization"));
        assert_eq!(headers["authorization"], "Bearer test-token");
        assert_eq!(headers["x-ghost-mode"], "true");
        assert_eq!(headers["connect-protocol-version"], "1");
    }
}
