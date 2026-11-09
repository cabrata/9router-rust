//! Port of open-sse/services/thoughtSignatureStore.js.
//!
//! Gemini thought signatures are stored per `tool_call_id` (optionally namespaced by
//! `sessionId`) so a signature produced by one turn can be replayed on the next. Two
//! tiers, exactly as in JS: a RAM cache with a 1 hour TTL, and a SQLite `kv` row with
//! a 7 day TTL that is written asynchronously and read back on a RAM miss.
//!
//! `nr-common` sits below `nr-db` in the crate graph, so the persistent tier is a
//! pluggable [`SignatureKv`] installed at startup by the db/server crate:
//! `shortcut: kv persistence is installed via install_persistence(); until a crate
//! below nr-db wires it, only the RAM tier is active — upgrade when nr-server boots`.

use indexmap::IndexMap;
use nr_core::{json::now_ms, Json};
use serde_json::json;
use std::sync::{Arc, LazyLock, Mutex};

const MAX_SIGNATURES: usize = 2000;
const MAX_PERSISTED_SIGNATURES: usize = 10_000;
const MEMORY_TTL_MS: i64 = 1000 * 60 * 60; // 1 hour
const PERSISTED_TTL_MS: i64 = 1000 * 60 * 60 * 24 * 7; // 7 days
const SCOPE: &str = "gemini_thought_signatures";

/// Persistent tier behind the RAM cache. Methods must swallow their own errors: the
/// JS kv calls are `.catch(() => {})` and the whole store is fail-open.
pub trait SignatureKv: Send + Sync {
    fn get(&self, key: &str) -> Option<Json>;
    fn set(&self, key: &str, value: Json);
    fn remove(&self, key: &str);
    fn get_all(&self) -> nr_core::JsonMap;
}

static PERSISTENCE: LazyLock<Mutex<Option<Arc<dyn SignatureKv>>>> = LazyLock::new(|| Mutex::new(None));

/// Install the SQLite-backed tier (called once at startup).
pub fn install_persistence(kv: Arc<dyn SignatureKv>) {
    if let Ok(mut slot) = PERSISTENCE.lock() {
        *slot = Some(kv);
    }
}

/// The kv scope these rows live under (`gemini_thought_signatures`).
pub fn scope() -> &'static str {
    SCOPE
}

struct Entry {
    signature: String,
    family: Option<String>,
    expires_at: i64,
}

/// `IndexMap` keeps insertion order, which is what the JS `Map` iteration over
/// `memorySignatures.keys()` relies on for oldest-first eviction.
static MEMORY: LazyLock<Mutex<IndexMap<String, Entry>>> =
    LazyLock::new(|| Mutex::new(IndexMap::new()));
static PRUNE_COUNTER: LazyLock<Mutex<u64>> = LazyLock::new(|| Mutex::new(0));

/// Model family that produced / will consume a signature. Antigravity serves Gemini and
/// Claude models behind the same API, and each backend only accepts its own signatures:
/// a Claude signature replayed to Gemini fails with 400 "Corrupted thought signature."
/// (and vice versa).
pub fn signature_family(model: Option<&str>) -> Option<String> {
    let m = model.unwrap_or_default().to_lowercase();
    if m.is_empty() {
        return None;
    }
    if m.contains("claude") {
        return Some("claude".to_string());
    }
    if m.contains("gemini") {
        return Some("gemini".to_string());
    }
    Some(m)
}

/// Entries stored before families were recorded (no `family`) stay usable for any model.
fn is_compatible(entry_family: Option<&str>, family: Option<&str>) -> bool {
    entry_family.is_none() || family.is_none() || entry_family == family
}

fn prune_memory_expired() {
    let now = now_ms();
    let Ok(mut map) = MEMORY.lock() else { return };
    map.retain(|_, v| v.expires_at > now);
    while map.len() > MAX_SIGNATURES {
        // `shift_remove_index(0)` is the oldest inserted key.
        if map.shift_remove_index(0).is_none() {
            break;
        }
    }
}

fn maybe_prune_persisted() {
    let count = match PRUNE_COUNTER.lock() {
        Ok(mut c) => {
            *c += 1;
            *c
        }
        Err(_) => return,
    };
    if count % 100 != 0 {
        return;
    }

    let Some(kv) = PERSISTENCE.lock().ok().and_then(|g| g.clone()) else {
        return;
    };

    let all = kv.get_all();
    let now = now_ms();

    let mut expired = Vec::new();
    let mut valid = Vec::new();
    for (key, entry) in &all {
        let signature_ok = entry.get("signature").and_then(Json::as_str).is_some_and(|s| !s.is_empty());
        let expires_at = entry.get("expiresAt").and_then(Json::as_i64);
        let is_expired = expires_at.is_some_and(|e| e <= now);
        if !signature_ok || is_expired {
            expired.push(key.clone());
        } else {
            valid.push((key.clone(), entry.get("createdAt").and_then(Json::as_i64).unwrap_or(0)));
        }
    }

    for key in expired {
        kv.remove(&key);
    }

    if valid.len() > MAX_PERSISTED_SIGNATURES {
        // Newest first, then drop the tail.
        valid.sort_by(|a, b| b.1.cmp(&a.1));
        for (key, _) in valid.into_iter().skip(MAX_PERSISTED_SIGNATURES) {
            kv.remove(&key);
        }
    }
}

/// Store a thought signature for a `tool_call_id` with an optional `sessionId`
/// namespace (RAM + SQLite async). `model` is the model that produced the signature;
/// lookups for another model family skip it.
pub fn store(tool_call_id: &str, signature: &str, session_id: Option<&str>, model: Option<&str>) {
    if tool_call_id.is_empty() || signature.is_empty() {
        return;
    }

    let now = now_ms();
    let family = signature_family(model);
    prune_memory_expired();

    let mut keys = Vec::new();
    if let Some(sid) = session_id.filter(|s| !s.is_empty()) {
        keys.push(format!("{sid}:{tool_call_id}"));
    }
    keys.push(tool_call_id.to_string());

    if let Ok(mut memory) = MEMORY.lock() {
        for k in &keys {
            memory.insert(
                k.clone(),
                Entry {
                    signature: signature.to_string(),
                    family: family.clone(),
                    expires_at: now + MEMORY_TTL_MS,
                },
                // Map.set on an existing key keeps its original position in JS.
            );
        }
    }

    if let Some(kv) = PERSISTENCE.lock().ok().and_then(|g| g.clone()) {
        for k in &keys {
            kv.set(
                k,
                json!({
                    "signature": signature,
                    "family": family,
                    "createdAt": now,
                    "expiresAt": now + PERSISTED_TTL_MS,
                }),
            );
        }
    }

    maybe_prune_persisted();
}

fn lookup_memory(key: &str, family: Option<&str>) -> Option<String> {
    let now = now_ms();
    let memory = MEMORY.lock().ok()?;
    let entry = memory.get(key)?;
    if entry.expires_at > now && is_compatible(entry.family.as_deref(), family) {
        return Some(entry.signature.clone());
    }
    None
}

fn remember_from_row(key: &str, row: &Json) -> Option<String> {
    let signature = row.get("signature").and_then(Json::as_str)?;
    if signature.is_empty() {
        return None;
    }
    if let Ok(mut memory) = MEMORY.lock() {
        memory.insert(
            key.to_string(),
            Entry {
                signature: signature.to_string(),
                family: row.get("family").and_then(Json::as_str).map(str::to_string),
                expires_at: now_ms() + MEMORY_TTL_MS,
            },
        );
    }
    Some(signature.to_string())
}

/// Retrieve a thought signature by `tool_call_id` (RAM first, then SQLite fallback).
/// `model` is the target model; signatures produced by another model family are
/// ignored. The JS function is async only because the kv adapter is async; the Rust
/// persistent tier is synchronous, so this is a plain call.
pub fn get(tool_call_id: &str, session_id: Option<&str>, model: Option<&str>) -> Option<String> {
    if tool_call_id.is_empty() {
        return None;
    }

    let family = signature_family(model);
    prune_memory_expired();

    let session_key = session_id
        .filter(|s| !s.is_empty())
        .map(|sid| format!("{sid}:{tool_call_id}"));

    if let Some(key) = &session_key {
        if let Some(sig) = lookup_memory(key, family.as_deref()) {
            return Some(sig);
        }
    }
    if let Some(sig) = lookup_memory(tool_call_id, family.as_deref()) {
        return Some(sig);
    }

    let Some(kv) = PERSISTENCE.lock().ok().and_then(|g| g.clone()) else {
        return None;
    };

    if let Some(key) = &session_key {
        if let Some(row) = kv.get(key) {
            let signature_ok = row.get("signature").and_then(Json::as_str).is_some_and(|s| !s.is_empty());
            let not_expired = row
                .get("expiresAt")
                .and_then(Json::as_i64)
                .map(|e| e > now_ms())
                .unwrap_or(true);
            if signature_ok && not_expired && is_compatible(row.get("family").and_then(Json::as_str), family.as_deref()) {
                return remember_from_row(key, &row);
            }
        }
    }

    if let Some(row) = kv.get(tool_call_id) {
        if let Some(expires_at) = row.get("expiresAt").and_then(Json::as_i64) {
            if expires_at <= now_ms() {
                kv.remove(tool_call_id);
                return None;
            }
        }
        if row.get("signature").and_then(Json::as_str).is_none() {
            return None;
        }
        if !is_compatible(row.get("family").and_then(Json::as_str), family.as_deref()) {
            return None;
        }
        return remember_from_row(tool_call_id, &row);
    }

    None
}

/// Synchronous get from the RAM cache only (for sync translators). `model` is the
/// target model; signatures produced by another model family are ignored.
pub fn get_sync(tool_call_id: &str, session_id: Option<&str>, model: Option<&str>) -> Option<String> {
    if tool_call_id.is_empty() {
        return None;
    }

    let family = signature_family(model);
    prune_memory_expired();

    if let Some(sid) = session_id.filter(|s| !s.is_empty()) {
        if let Some(sig) = lookup_memory(&format!("{sid}:{tool_call_id}"), family.as_deref()) {
            return Some(sig);
        }
    }
    lookup_memory(tool_call_id, family.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    #[derive(Default)]
    struct FakeKv(StdMutex<nr_core::JsonMap>);
    impl SignatureKv for FakeKv {
        fn get(&self, key: &str) -> Option<Json> {
            self.0.lock().ok()?.get(key).cloned()
        }
        fn set(&self, key: &str, value: Json) {
            if let Ok(mut m) = self.0.lock() {
                m.insert(key.to_string(), value);
            }
        }
        fn remove(&self, key: &str) {
            if let Ok(mut m) = self.0.lock() {
                m.shift_remove(key);
            }
        }
        fn get_all(&self) -> nr_core::JsonMap {
            self.0.lock().map(|m| m.clone()).unwrap_or_default()
        }
    }

    /// Serializes the tests that share the process-global store.
    static TEST_LOCK: StdMutex<()> = StdMutex::new(());

    #[test]
    fn family_detection() {
        assert_eq!(signature_family(Some("Claude-Opus")), Some("claude".to_string()));
        assert_eq!(signature_family(Some("gemini-3-pro")), Some("gemini".to_string()));
        assert_eq!(signature_family(Some("Other")), Some("other".to_string()));
        assert_eq!(signature_family(None), None);
    }

    #[test]
    fn roundtrip_with_session_namespace() {
        let _g = TEST_LOCK.lock().unwrap();
        store("call_1", "sig-1", Some("sess"), Some("gemini-3-pro"));
        assert_eq!(get_sync("call_1", Some("sess"), Some("gemini-3-pro")), Some("sig-1".into()));
        assert_eq!(get_sync("call_1", None, Some("gemini-3-pro")), Some("sig-1".into()));
        // A different family must not see it.
        assert_eq!(get_sync("call_1", None, Some("claude-opus")), None);
        // A different session still finds the unnamespaced key.
        assert_eq!(get_sync("call_1", Some("other"), Some("gemini-3-pro")), Some("sig-1".into()));
    }

    #[test]
    fn empty_inputs_are_ignored() {
        let _g = TEST_LOCK.lock().unwrap();
        store("", "sig", None, None);
        store("call_x", "", None, None);
        assert_eq!(get_sync("", None, None), None);
    }

    #[test]
    fn persisted_tier_is_used_on_ram_miss() {
        let _g = TEST_LOCK.lock().unwrap();
        let kv = Arc::new(FakeKv::default());
        kv.set(
            "call_p",
            json!({"signature": "sig-p", "family": "gemini", "createdAt": 1, "expiresAt": now_ms() + 60_000}),
        );
        install_persistence(kv.clone());

        assert_eq!(get_sync("call_p", None, Some("gemini-x")), None);
        assert_eq!(get("call_p", None, Some("gemini-x")), Some("sig-p".into()));
        // Promoted into RAM: a subsequent sync read hits.
        assert_eq!(get_sync("call_p", None, Some("gemini-x")), Some("sig-p".into()));

        // Wrong family is rejected even from the persistent tier.
        assert_eq!(get("call_p", None, Some("claude-opus")), None);
    }

    #[test]
    fn expired_persisted_row_is_removed() {
        let _g = TEST_LOCK.lock().unwrap();
        let kv = Arc::new(FakeKv::default());
        kv.set("call_exp", json!({"signature": "old", "expiresAt": now_ms() - 1000}));
        install_persistence(kv.clone());

        assert_eq!(get("call_exp", None, None), None);
        assert!(kv.get("call_exp").is_none());
    }
}
