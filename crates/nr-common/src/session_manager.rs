use sha2::{Sha256, Digest};
use std::collections::HashMap;
use std::sync::{Mutex, LazyLock};
use uuid::Uuid;

// Session entry: { sessionId, lastUsed }
#[derive(Clone, Debug)]
struct SessionEntry {
    session_id: String,
    last_used: u64,
}

#[derive(Clone, Debug)]
struct ContinuationEntry {
    continuation_id: String,
    last_used: u64,
}

// Global runtime storage for session IDs (per connection)
static RUNTIME_SESSION_STORE: LazyLock<Mutex<HashMap<String, SessionEntry>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static CONTINUATION_STORE: LazyLock<Mutex<HashMap<String, ContinuationEntry>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static ASSISTANT_SESSION_STORE: LazyLock<Mutex<HashMap<String, SessionEntry>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

const MAX_SESSIONS: usize = 1000;
const MAX_ASSISTANT_SESSIONS: usize = 5000;
const MAX_CONTINUATION_SESSIONS: usize = 5000;
const ASSISTANT_MIN_LEN: usize = 50;
const ASSISTANT_CAP_LEN: usize = 50;

const SESSION_HEADER_KEYS: &[&str] = &["x-session-id", "session-id", "session_id", "x-amp-thread-id"];
const CLAUDE_CODE_SESSION_HEADER: &str = "x-claude-code-session-id";

/// Generate a Session ID using binary's exact logic: UUID + timestamp
pub fn generate_binary_style_id() -> String {
    format!("{}{}", Uuid::new_v4(), chrono::Utc::now().timestamp_millis())
}

/// Get or create a session ID for the given connection
pub fn derive_session_id(connection_id: &str) -> String {
    if connection_id.is_empty() {
        return generate_binary_style_id();
    }

    let mut store = RUNTIME_SESSION_STORE.lock().unwrap();
    let now = chrono::Utc::now().timestamp_millis() as u64;

    if let Some(entry) = store.get_mut(connection_id) {
        entry.last_used = now;
        return entry.session_id.clone();
    }

    // Evict oldest if at capacity
    if store.len() >= MAX_SESSIONS {
        if let Some(oldest_key) = store.keys().next().cloned() {
            store.remove(&oldest_key);
        }
    }

    let session_id = generate_binary_style_id();
    store.insert(connection_id.to_string(), SessionEntry { session_id: session_id.clone(), last_used: now });
    session_id
}

/// Clear all session IDs
pub fn clear_session_store() {
    RUNTIME_SESSION_STORE.lock().unwrap().clear();
    CONTINUATION_STORE.lock().unwrap().clear();
    ASSISTANT_SESSION_STORE.lock().unwrap().clear();
}

/// Normalize session ID: trim, length cap (256 chars max)
fn normalize_session_id(value: &str) -> Option<String> {
    let v = value.trim();
    if v.is_empty() || v.len() > 256 {
        None
    } else {
        Some(v.to_string())
    }
}

/// Extract Claude Code session ID from metadata.user_id
fn extract_claude_code_session(user_id: &str) -> Option<String> {
    // Try regex match: _session_{uuid}
    if user_id.contains("_session_") {
        if let Some(start) = user_id.find("_session_") {
            let after = &user_id[start + 9..];
            if let Some(end) = after.find(|c: char| !c.is_alphanumeric() && c != '-') {
                return normalize_session_id(&after[..end]);
            } else {
                return normalize_session_id(after);
            }
        }
    }

    // Try JSON parse
    if user_id.starts_with('{') {
        if let Ok(obj) = serde_json::from_str::<serde_json::Value>(user_id) {
            if let Some(sid) = obj.get("session_id").and_then(|v| v.as_str()) {
                return normalize_session_id(sid);
            }
        }
    }

    None
}

/// Extract session ID from headers
fn header_value(headers: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    if let Some(val) = headers.get(key).or_else(|| headers.get(&key.to_lowercase())) {
        if let Some(s) = val.as_str() {
            return normalize_session_id(s);
        }
    }
    None
}

/// Extract Antigravity session ID from request body
fn extract_antigravity_session(body: &serde_json::Value) -> Option<String> {
    if let Some(sid) = body.get("request").and_then(|r| r.get("sessionId")) {
        if let Some(s) = sid.as_str() {
            return normalize_session_id(s);
        }
    }

    if let Some(req_id) = body.get("requestId").and_then(|v| v.as_str()) {
        // Pattern: <type>/<uuid>/...
        if let Some(slash1) = req_id.find('/') {
            if let Some(slash2) = req_id[slash1 + 1..].find('/') {
                let uuid_part = &req_id[slash1 + 1..slash1 + 1 + slash2];
                return normalize_session_id(uuid_part);
            }
        }
    }

    None
}

/// Extract client session ID from headers/body
fn extract_client_session_id(headers: Option<&serde_json::Map<String, serde_json::Value>>, body: &serde_json::Value, scope: &str) -> Option<String> {
    // Claude Code session
    let user_id = body.get("metadata").and_then(|m| m.get("user_id")).and_then(|v| v.as_str());
    let claude = user_id.and_then(|uid| extract_claude_code_session(uid))
        .or_else(|| headers.and_then(|h| header_value(h, CLAUDE_CODE_SESSION_HEADER)));
    
    if let Some(c) = claude {
        return Some(format!("claude:{}", c));
    }

    // Antigravity session
    if let Some(ag) = extract_antigravity_session(body) {
        return Some(format!("antigravity:{}", ag));
    }

    // Standard headers
    if let Some(h) = headers {
        for key in SESSION_HEADER_KEYS {
            if let Some(v) = header_value(h, key) {
                return Some(v);
            }
        }

        if scope != "kiro" {
            if let Some(req_id) = header_value(h, "x-client-request-id") {
                return Some(req_id);
            }
        }
    }

    // Body fields
    if let Some(pck) = body.get("prompt_cache_key").and_then(|v| v.as_str()) {
        if let Some(n) = normalize_session_id(pck) {
            return Some(n);
        }
    }
    if let Some(sid) = body.get("session_id").and_then(|v| v.as_str()) {
        if let Some(n) = normalize_session_id(sid) {
            return Some(n);
        }
    }
    if let Some(cid) = body.get("conversation_id").and_then(|v| v.as_str()) {
        if let Some(n) = normalize_session_id(cid) {
            return Some(n);
        }
    }
    if scope != "kiro" {
        if let Some(uid) = body.get("metadata").and_then(|m| m.get("user_id")).and_then(|v| v.as_str()) {
            if let Some(n) = normalize_session_id(uid) {
                return Some(n);
            }
        }
    }

    None
}

#[derive(Debug, Clone)]
pub struct SessionIdentity {
    pub session_id: String,
    pub ephemeral: bool,
}

/// Resolve a conversation-stable session ID
pub fn resolve_session_identity(
    headers: Option<&serde_json::Map<String, serde_json::Value>>,
    body: &serde_json::Value,
    connection_id: Option<&str>,
    workspace_id: Option<&str>,
    scope: &str,
) -> SessionIdentity {
    if let Some(client) = extract_client_session_id(headers, body, scope) {
        return SessionIdentity { session_id: client, ephemeral: false };
    }

    if scope != "kiro" {
        if let Some(from_assistant) = assistant_text_session_id(scope, connection_id, body) {
            return SessionIdentity { session_id: from_assistant, ephemeral: false };
        }
    }

    if let Some(ws) = workspace_id.and_then(|w| normalize_session_id(w)) {
        return SessionIdentity { session_id: ws, ephemeral: false };
    }

    if scope == "kiro" {
        return SessionIdentity { session_id: generate_binary_style_id(), ephemeral: true };
    }

    SessionIdentity {
        session_id: derive_session_id(connection_id.unwrap_or("")),
        ephemeral: false,
    }
}

/// Resolve just the session ID
pub fn resolve_session_id(
    headers: Option<&serde_json::Map<String, serde_json::Value>>,
    body: &serde_json::Value,
    connection_id: Option<&str>,
    workspace_id: Option<&str>,
    scope: &str,
) -> String {
    resolve_session_identity(headers, body, connection_id, workspace_id, scope).session_id
}

/// Resolve continuation ID
pub fn resolve_continuation_id(
    session_id: &str,
    connection_id: Option<&str>,
    scope: &str,
    ephemeral: bool,
) -> String {
    if ephemeral {
        return Uuid::new_v4().to_string();
    }

    let key = format!("{}:{}:{}", scope, connection_id.unwrap_or(""), session_id);
    let mut store = CONTINUATION_STORE.lock().unwrap();
    let now = chrono::Utc::now().timestamp_millis() as u64;

    if let Some(entry) = store.get(&key) {
        return entry.continuation_id.clone();
    }

    if store.len() >= MAX_CONTINUATION_SESSIONS {
        if let Some(oldest_key) = store.keys().next().cloned() {
            store.remove(&oldest_key);
        }
    }

    let continuation_id = Uuid::new_v4().to_string();
    store.insert(key, ContinuationEntry { continuation_id: continuation_id.clone(), last_used: now });
    continuation_id
}

/// Convert any session ID to Antigravity numeric format
pub fn to_numeric_session_id(session_id: &str) -> Option<String> {
    let v = normalize_session_id(session_id)?;
    
    // Already numeric
    if v.chars().all(|c| c.is_ascii_digit() || c == '-') {
        return Some(v);
    }

    // Hash to numeric
    let mut hasher = Sha256::new();
    hasher.update(v.as_bytes());
    let hash = hasher.finalize();
    
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&hash[..8]);
    let n = u64::from_be_bytes(bytes) & 0x7fffffffffffffff;
    
    Some(format!("-{}", n))
}

/// Accumulate assistant text from messages
fn accumulate_assistant_text(body: &serde_json::Value) -> String {
    let mut text = String::new();
    
    let messages = body.get("messages")
        .or_else(|| body.get("input"))
        .and_then(|v| v.as_array());

    if let Some(msgs) = messages {
        for msg in msgs {
            if msg.get("role").and_then(|v| v.as_str()) != Some("assistant") {
                continue;
            }

            if let Some(content) = msg.get("content") {
                if let Some(s) = content.as_str() {
                    text.push_str(s);
                } else if let Some(arr) = content.as_array() {
                    for item in arr {
                        if let Some(txt) = item.get("text").and_then(|v| v.as_str()) {
                            text.push_str(txt);
                        } else if let Some(out) = item.get("output").and_then(|v| v.as_str()) {
                            text.push_str(out);
                        }
                    }
                }
            }

            if text.len() >= ASSISTANT_CAP_LEN {
                break;
            }
        }
    }

    text
}

/// Get session ID keyed on assistant text
fn assistant_text_session_id(scope: &str, connection_id: Option<&str>, body: &serde_json::Value) -> Option<String> {
    let text = accumulate_assistant_text(body);
    if text.len() < ASSISTANT_MIN_LEN {
        return None;
    }

    let mut hasher = Sha256::new();
    hasher.update(format!("{}:{}", scope, &text[..ASSISTANT_CAP_LEN.min(text.len())]).as_bytes());
    let hash_hex = format!("{:x}", hasher.finalize());
    let hash = &hash_hex[..16.min(hash_hex.len())];

    let mut store = ASSISTANT_SESSION_STORE.lock().unwrap();
    let now = chrono::Utc::now().timestamp_millis() as u64;

    if let Some(entry) = store.get_mut(hash) {
        entry.last_used = now;
        return Some(entry.session_id.clone());
    }

    if store.len() >= MAX_ASSISTANT_SESSIONS {
        if let Some(oldest_key) = store.keys().next().cloned() {
            store.remove(&oldest_key);
        }
    }

    let session_id = generate_binary_style_id();
    store.insert(hash.to_string(), SessionEntry { session_id: session_id.clone(), last_used: now });
    Some(session_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_binary_style_id() {
        let id = generate_binary_style_id();
        assert!(id.len() > 20);
    }

    #[test]
    fn test_derive_session_id() {
        clear_session_store();
        let id1 = derive_session_id("conn1");
        let id2 = derive_session_id("conn1");
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_normalize_session_id() {
        assert_eq!(normalize_session_id("  test  "), Some("test".to_string()));
        assert_eq!(normalize_session_id(""), None);
        assert_eq!(normalize_session_id(&"a".repeat(300)), None);
    }

    #[test]
    fn test_to_numeric_session_id() {
        assert_eq!(to_numeric_session_id("123"), Some("123".to_string()));
        assert_eq!(to_numeric_session_id("-456"), Some("-456".to_string()));
        let result = to_numeric_session_id("abc-def-ghi");
        assert!(result.is_some());
        assert!(result.unwrap().starts_with("-"));
    }
}
