//! Port of open-sse/services/systemoneRouter.js
//!
//! Pure System One router utilities for intelligent combo model selection.
//! Used by the "smart" combo strategy to pick the best model for a request
//! before generation begins, with session stickiness for prompt-cache retention.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use nr_core::Json;
use serde_json::json;

pub const DEFAULT_SYSTEMONE_ROUTER_MODEL: &str = "oc/jev-1.13-free";
pub const DEFAULT_SYSTEMONE_TIMEOUT_MS: u64 = 8000;
pub const MAX_STATE_CHARS: usize = 2000;
pub const MAX_STICKY_SESSIONS: usize = 5000;
/// 2 hours
pub const STICKY_SESSION_TTL_MS: i64 = 2 * 60 * 60 * 1000;

#[derive(Clone, Debug)]
struct StickyEntry {
    model: String,
    last_used: i64,
}

/// In-memory session stickiness store for smart-routed combos.
/// Key: `{comboName}:{sessionId}` -> `{ model, lastUsed }`
static COMBO_SESSION_MODEL_STORE: LazyLock<Mutex<HashMap<String, StickyEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Lock the store, recovering from a poisoned mutex.
///
/// The store is a pure memo: a panic in another request must not turn every
/// later routing decision into a panic. JS has no poisoning either.
fn store() -> std::sync::MutexGuard<'static, HashMap<String, StickyEntry>> {
    COMBO_SESSION_MODEL_STORE.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_ms() -> i64 {
    nr_core::json::now_ms()
}

/// Truncate to `max_chars` characters (JS `String#slice` is by UTF-16 code unit;
/// chars are close enough and never split a UTF-8 sequence).
fn truncate_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// Flatten a message `content` (string or array of parts) to text.
fn extract_text_from_content(content: Option<&Json>) -> String {
    match content {
        Some(Json::String(s)) => s.clone(),
        Some(Json::Array(parts)) => parts
            .iter()
            .map(|part| {
                if let Json::String(s) = part {
                    return s.clone();
                }
                if part.get("type").and_then(Json::as_str) == Some("text") {
                    return part.get("text").and_then(Json::as_str).unwrap_or("").to_string();
                }
                String::new()
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// `extractPromptState` — the last user turn (or prompt), truncated.
pub fn extract_prompt_state(body: &Json, max_chars: usize) -> String {
    if !body.is_object() {
        return String::new();
    }

    // Messages array (OpenAI / Claude / etc.) or input array (Codex)
    let messages = body
        .get("messages")
        .and_then(Json::as_array)
        .or_else(|| body.get("input").and_then(Json::as_array));

    if let Some(messages) = messages.filter(|m| !m.is_empty()) {
        // Find the latest user message
        for msg in messages.iter().rev() {
            let role = msg.get("role").and_then(Json::as_str);
            let kind = msg.get("type").and_then(Json::as_str);
            let has_content = msg.get("content").map(|c| !c.is_null()).unwrap_or(false);
            let is_candidate = role == Some("user")
                || kind == Some("message")
                || (role.is_none() && has_content);
            if !is_candidate {
                continue;
            }
            let content = msg.get("content").or_else(|| msg.get("text"));
            let text = extract_text_from_content(content);
            if !text.trim().is_empty() {
                return truncate_chars(text.trim(), max_chars);
            }
        }
        // Fallback: latest message of any role
        if let Some(last_msg) = messages.last() {
            let content = last_msg.get("content").or_else(|| last_msg.get("text"));
            let text = extract_text_from_content(content);
            if !text.is_empty() {
                return truncate_chars(text.trim(), max_chars);
            }
        }
    }

    // String prompt / input
    if let Some(prompt) = body.get("prompt").and_then(Json::as_str) {
        if !prompt.trim().is_empty() {
            return truncate_chars(prompt.trim(), max_chars);
        }
    }
    if let Some(input) = body.get("input").and_then(Json::as_str) {
        if !input.trim().is_empty() {
            return truncate_chars(input.trim(), max_chars);
        }
    }

    String::new()
}

/// `buildChoiceCriteria`: keys are model names, values `null` so System One uses
/// its native knowledge of models without manual descriptions.
pub fn build_choice_criteria(models: &[String]) -> Json {
    let mut map = serde_json::Map::new();
    for m in models {
        let trimmed = m.trim();
        if !trimmed.is_empty() {
            map.insert(trimmed.to_string(), Json::Null);
        }
    }
    Json::Object(map)
}

#[derive(Debug, Clone, PartialEq)]
pub struct SystemOneChoice {
    pub choice: String,
    pub confidence: Option<f64>,
    pub probabilities: Option<HashMap<String, f64>>,
}

/// `parseSystemOneRouteAnswer` — the route answer, or `None` when there is no
/// usable `choice`.
pub fn parse_systemone_route_answer(json: &Json) -> Option<SystemOneChoice> {
    if !json.is_object() {
        return None;
    }
    let route = json.get("answers")?.get("route")?;
    if !route.is_object() {
        return None;
    }

    let raw_choice = route
        .get("choice")
        .and_then(Json::as_str)
        .map(str::trim)
        .unwrap_or("");
    if raw_choice.is_empty() {
        return None;
    }

    let confidence = route
        .get("confidence")
        .and_then(Json::as_f64)
        .filter(|c| c.is_finite());

    let probabilities = route
        .get("probabilities")
        .and_then(Json::as_object)
        .map(|probs| {
            probs
                .iter()
                .filter_map(|(k, v)| {
                    // JS coerces with Number(v): numbers pass through, numeric
                    // strings parse, everything else is dropped.
                    let num = match v {
                        Json::Number(n) => n.as_f64(),
                        Json::String(s) => s.trim().parse::<f64>().ok(),
                        _ => None,
                    }?;
                    if num.is_finite() {
                        Some((k.clone(), num))
                    } else {
                        None
                    }
                })
                .collect::<HashMap<String, f64>>()
        });

    Some(SystemOneChoice { choice: raw_choice.to_string(), confidence, probabilities })
}

/// `orderModelsByChoice`:
/// 1. The selected choice is placed at index 0.
/// 2. Remaining models are ordered by probabilities descending.
/// 3. Models without probabilities retain their relative order at the end.
pub fn order_models_by_choice(models: &[String], choice_result: Option<&SystemOneChoice>) -> Vec<String> {
    if models.len() <= 1 {
        return models.to_vec();
    }
    let choice = match choice_result {
        Some(c) if !c.choice.is_empty() && models.contains(&c.choice) => c,
        _ => return models.to_vec(),
    };

    let mut others: Vec<String> = models.iter().filter(|m| **m != choice.choice).cloned().collect();

    if let Some(probabilities) = &choice.probabilities {
        // Stable: JS Array#sort is stable, so equal probabilities keep combo order.
        others.sort_by(|a, b| {
            let p_a = probabilities.get(a).copied().unwrap_or(0.0);
            let p_b = probabilities.get(b).copied().unwrap_or(0.0);
            p_b.partial_cmp(&p_a).unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    let mut out = Vec::with_capacity(models.len());
    out.push(choice.choice.clone());
    out.extend(others);
    out
}

fn sticky_key(combo_name: &str, session_id: &str) -> String {
    format!("{combo_name}:{session_id}")
}

/// `getStickyComboModel` — the stored model when still valid, else `None`.
pub fn get_sticky_combo_model(combo_name: &str, session_id: Option<&str>, models: &[String]) -> Option<String> {
    let session_id = session_id.filter(|s| !s.is_empty())?;
    if combo_name.is_empty() {
        return None;
    }
    let key = sticky_key(combo_name, session_id);
    let mut store = store();
    let entry = store.get(&key)?;

    // Check TTL (2 hours)
    if now_ms() - entry.last_used > STICKY_SESSION_TTL_MS {
        store.remove(&key);
        return None;
    }

    // Ensure stored model still belongs to combo
    if !models.contains(&entry.model) {
        store.remove(&key);
        return None;
    }

    let model = entry.model.clone();
    if let Some(entry) = store.get_mut(&key) {
        entry.last_used = now_ms();
    }
    Some(model)
}

/// `setStickyComboModel` — record the model so later turns keep the prompt cache warm.
pub fn set_sticky_combo_model(combo_name: &str, session_id: Option<&str>, model: &str) {
    let session_id = match session_id.filter(|s| !s.is_empty()) {
        Some(s) => s,
        None => return,
    };
    if combo_name.is_empty() || model.is_empty() {
        return;
    }

    prune_sticky_combo_sessions();
    let key = sticky_key(combo_name, session_id);
    store().insert(key, StickyEntry { model: model.to_string(), last_used: now_ms() });
}

/// `clearStickyComboSession` — clear one session, or all sessions for a combo
/// when `session_id` is `None`; clear everything when `combo_name` is empty.
pub fn clear_sticky_combo_session(combo_name: &str, session_id: Option<&str>) {
    let mut store = store();
    if combo_name.is_empty() {
        store.clear();
        return;
    }
    if let Some(session_id) = session_id.filter(|s| !s.is_empty()) {
        store.remove(&sticky_key(combo_name, session_id));
        return;
    }
    // Clear all for this combo
    let prefix = format!("{combo_name}:");
    store.retain(|k, _| !k.starts_with(&prefix));
}

/// `pruneStickyComboSessions` — drop expired entries, then the oldest quarter
/// when still at the ceiling.
pub fn prune_sticky_combo_sessions() {
    let mut store = store();
    if store.len() < MAX_STICKY_SESSIONS {
        return;
    }

    let now = now_ms();
    // Remove expired
    store.retain(|_, v| now - v.last_used <= STICKY_SESSION_TTL_MS);

    // If still above ceiling, drop oldest entries
    if store.len() >= MAX_STICKY_SESSIONS {
        let mut entries: Vec<(String, i64)> =
            store.iter().map(|(k, v)| (k.clone(), v.last_used)).collect();
        entries.sort_by_key(|(_, last_used)| *last_used);
        let drop_count = MAX_STICKY_SESSIONS / 4;
        for (k, _) in entries.into_iter().take(drop_count) {
            store.remove(&k);
        }
    }
}

/// `resetStickyComboStore` — for testing.
pub fn reset_sticky_combo_store() {
    store().clear();
}

/// The JS `criteria` object for one route request, plus the state payload that
/// travels with it (`{ state, criteria }`). Callers send this to System One.
pub fn build_route_request(state: &str, models: &[String]) -> Json {
    json!({ "state": state, "criteria": build_choice_criteria(models) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The sticky store is process-global, so the tests that touch it must not
    /// interleave (each one resets and asserts on the whole map).
    static STICKY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn sticky_guard() -> std::sync::MutexGuard<'static, ()> {
        STICKY_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn models(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn extract_prompt_prefers_latest_user_message() {
        let body = json!({
            "messages": [
                { "role": "user", "content": "first" },
                { "role": "assistant", "content": "reply" },
                { "role": "user", "content": "second" },
            ]
        });
        assert_eq!(extract_prompt_state(&body, MAX_STATE_CHARS), "second");
    }

    #[test]
    fn extract_prompt_joins_text_parts() {
        let body = json!({
            "messages": [{ "role": "user", "content": [
                { "type": "text", "text": "hello" },
                { "type": "image_url", "image_url": { "url": "x" } },
                { "type": "text", "text": "world" },
            ]}]
        });
        assert_eq!(extract_prompt_state(&body, MAX_STATE_CHARS), "hello world");
    }

    #[test]
    fn extract_prompt_falls_back_to_last_message_of_any_role() {
        let body = json!({
            "messages": [
                { "role": "system", "content": "instructions" },
                { "role": "assistant", "content": "no user turn" },
            ]
        });
        assert_eq!(extract_prompt_state(&body, MAX_STATE_CHARS), "no user turn");
    }

    #[test]
    fn extract_prompt_reads_codex_input_array() {
        let body = json!({
            "input": [{ "type": "message", "role": "user", "content": "from codex" }]
        });
        assert_eq!(extract_prompt_state(&body, MAX_STATE_CHARS), "from codex");
    }

    #[test]
    fn extract_prompt_reads_string_prompt_and_input() {
        assert_eq!(
            extract_prompt_state(&json!({ "prompt": "  hi  " }), MAX_STATE_CHARS),
            "hi"
        );
        assert_eq!(
            extract_prompt_state(&json!({ "input": "  yo  " }), MAX_STATE_CHARS),
            "yo"
        );
    }

    #[test]
    fn extract_prompt_truncates_to_max_chars() {
        let body = json!({ "prompt": "abcdefghij" });
        assert_eq!(extract_prompt_state(&body, 4), "abcd");
    }

    #[test]
    fn extract_prompt_empty_cases() {
        assert_eq!(extract_prompt_state(&json!({}), MAX_STATE_CHARS), "");
        assert_eq!(extract_prompt_state(&json!("string"), MAX_STATE_CHARS), "");
        assert_eq!(
            extract_prompt_state(&json!({ "messages": [] }), MAX_STATE_CHARS),
            ""
        );
        assert_eq!(
            extract_prompt_state(&json!({ "prompt": "   " }), MAX_STATE_CHARS),
            ""
        );
    }

    #[test]
    fn build_choice_criteria_maps_models_to_null() {
        let criteria = build_choice_criteria(&models(&["oc/a", " oc/b ", "", "oc/a"]));
        assert_eq!(criteria, json!({ "oc/a": null, "oc/b": null }));
    }

    #[test]
    fn parse_route_answer_reads_choice_and_probabilities() {
        let payload = json!({
            "answers": {
                "route": {
                    "choice": " oc/b ",
                    "confidence": 0.75,
                    "probabilities": { "oc/a": 0.2, "oc/b": "0.75", "oc/c": true },
                }
            }
        });
        let parsed = parse_systemone_route_answer(&payload).expect("parses");
        assert_eq!(parsed.choice, "oc/b");
        assert_eq!(parsed.confidence, Some(0.75));
        let probs = parsed.probabilities.expect("probabilities");
        assert_eq!(probs.get("oc/a"), Some(&0.2));
        // Number("0.75") coerces; a boolean does not.
        assert_eq!(probs.get("oc/b"), Some(&0.75));
        assert!(!probs.contains_key("oc/c"));
    }

    #[test]
    fn parse_route_answer_requires_choice() {
        assert!(parse_systemone_route_answer(&json!({})).is_none());
        assert!(parse_systemone_route_answer(&json!({ "answers": {} })).is_none());
        assert!(parse_systemone_route_answer(&json!({ "answers": { "route": {} } })).is_none());
        assert!(
            parse_systemone_route_answer(&json!({ "answers": { "route": { "choice": "  " } } }))
                .is_none()
        );
    }

    #[test]
    fn parse_route_answer_without_probabilities() {
        let payload = json!({ "answers": { "route": { "choice": "oc/a" } } });
        let parsed = parse_systemone_route_answer(&payload).expect("parses");
        assert_eq!(parsed.confidence, None);
        assert!(parsed.probabilities.is_none());
    }

    #[test]
    fn order_models_puts_choice_first_then_probabilities() {
        let combo = models(&["oc/a", "oc/b", "oc/c"]);
        let choice = SystemOneChoice {
            choice: "oc/c".into(),
            confidence: None,
            probabilities: Some(
                [("oc/a".to_string(), 0.1), ("oc/b".to_string(), 0.6)]
                    .into_iter()
                    .collect(),
            ),
        };
        assert_eq!(
            order_models_by_choice(&combo, Some(&choice)),
            models(&["oc/c", "oc/b", "oc/a"])
        );
    }

    #[test]
    fn order_models_keeps_relative_order_without_probabilities() {
        let combo = models(&["oc/a", "oc/b", "oc/c"]);
        let choice = SystemOneChoice { choice: "oc/b".into(), confidence: None, probabilities: None };
        assert_eq!(
            order_models_by_choice(&combo, Some(&choice)),
            models(&["oc/b", "oc/a", "oc/c"])
        );
    }

    #[test]
    fn order_models_unchanged_for_unknown_choice() {
        let combo = models(&["oc/a", "oc/b"]);
        let choice =
            SystemOneChoice { choice: "oc/z".into(), confidence: None, probabilities: None };
        assert_eq!(order_models_by_choice(&combo, Some(&choice)), combo);
        assert_eq!(order_models_by_choice(&combo, None), combo);
    }

    #[test]
    fn order_models_single_and_empty() {
        assert_eq!(order_models_by_choice(&models(&["oc/a"]), None), models(&["oc/a"]));
        let empty: Vec<String> = vec![];
        assert!(order_models_by_choice(&empty, None).is_empty());
    }

    #[test]
    fn sticky_model_round_trip_and_membership_check() {
        let _guard = sticky_guard();
        reset_sticky_combo_store();
        let combo = models(&["oc/a", "oc/b"]);
        assert_eq!(get_sticky_combo_model("smart", Some("s1"), &combo), None);

        set_sticky_combo_model("smart", Some("s1"), "oc/b");
        assert_eq!(
            get_sticky_combo_model("smart", Some("s1"), &combo),
            Some("oc/b".to_string())
        );

        // Model dropped from the combo invalidates the entry.
        let narrowed = models(&["oc/a"]);
        assert_eq!(get_sticky_combo_model("smart", Some("s1"), &narrowed), None);
        // ...and the entry is gone, not just skipped.
        assert_eq!(
            get_sticky_combo_model("smart", Some("s1"), &combo),
            None
        );
        reset_sticky_combo_store();
    }

    #[test]
    fn sticky_model_requires_session_and_combo() {
        let _guard = sticky_guard();
        reset_sticky_combo_store();
        let combo = models(&["oc/a"]);
        set_sticky_combo_model("smart", None, "oc/a");
        assert_eq!(get_sticky_combo_model("smart", None, &combo), None);
        set_sticky_combo_model("", Some("s1"), "oc/a");
        assert_eq!(get_sticky_combo_model("", Some("s1"), &combo), None);
        set_sticky_combo_model("smart", Some("s1"), "");
        assert_eq!(get_sticky_combo_model("smart", Some("s1"), &combo), None);
        reset_sticky_combo_store();
    }

    #[test]
    fn sticky_model_expires_after_ttl() {
        let _guard = sticky_guard();
        reset_sticky_combo_store();
        let combo = models(&["oc/a"]);
        let key = sticky_key("smart", "s1");
        store().insert(
            key,
            StickyEntry { model: "oc/a".into(), last_used: now_ms() - STICKY_SESSION_TTL_MS - 1 },
        );
        assert_eq!(get_sticky_combo_model("smart", Some("s1"), &combo), None);
        reset_sticky_combo_store();
    }

    #[test]
    fn clear_sticky_sessions() {
        let _guard = sticky_guard();
        reset_sticky_combo_store();
        let combo = models(&["oc/a"]);
        set_sticky_combo_model("smart", Some("s1"), "oc/a");
        set_sticky_combo_model("smart", Some("s2"), "oc/a");
        set_sticky_combo_model("other", Some("s1"), "oc/a");

        // One session
        clear_sticky_combo_session("smart", Some("s1"));
        assert_eq!(get_sticky_combo_model("smart", Some("s1"), &combo), None);
        assert_eq!(
            get_sticky_combo_model("smart", Some("s2"), &combo),
            Some("oc/a".to_string())
        );

        // All sessions for a combo
        clear_sticky_combo_session("smart", None);
        assert_eq!(get_sticky_combo_model("smart", Some("s2"), &combo), None);
        assert_eq!(
            get_sticky_combo_model("other", Some("s1"), &combo),
            Some("oc/a".to_string())
        );

        // Everything
        clear_sticky_combo_session("", None);
        assert_eq!(get_sticky_combo_model("other", Some("s1"), &combo), None);
        reset_sticky_combo_store();
    }

    #[test]
    fn prune_is_noop_below_ceiling() {
        let _guard = sticky_guard();
        reset_sticky_combo_store();
        set_sticky_combo_model("smart", Some("s1"), "oc/a");
        prune_sticky_combo_sessions();
        assert_eq!(store().len(), 1);
        reset_sticky_combo_store();
    }

    #[test]
    fn prune_removes_expired_entry_when_exactly_at_ceiling() {
        let _guard = sticky_guard();
        reset_sticky_combo_store();
        let now = now_ms();
        {
            let mut store = store();
            // One expired entry plus (MAX-1) live ones == exactly MAX, so the
            // expiry pass trips the ceiling but leaves the store just under it.
            store.insert(
                "smart:expired".into(),
                StickyEntry { model: "oc/a".into(), last_used: now - STICKY_SESSION_TTL_MS - 1 },
            );
            for i in 0..MAX_STICKY_SESSIONS - 1 {
                store.insert(
                    format!("smart:s{i}"),
                    StickyEntry { model: "oc/a".into(), last_used: now - (i as i64) },
                );
            }
        }
        prune_sticky_combo_sessions();
        {
            let store = store();
            assert!(!store.contains_key("smart:expired"));
            assert_eq!(store.len(), MAX_STICKY_SESSIONS - 1);
        }
        reset_sticky_combo_store();
    }

    #[test]
    fn prune_drops_oldest_quarter_when_still_over_ceiling() {
        let _guard = sticky_guard();
        reset_sticky_combo_store();
        let now = now_ms();
        {
            let mut store = store();
            // All live, so only the quarter-drop branch fires. `last_used = now - i`
            // makes s0 the newest and s(MAX-1) the oldest.
            for i in 0..MAX_STICKY_SESSIONS {
                store.insert(
                    format!("smart:s{i}"),
                    StickyEntry { model: "oc/a".into(), last_used: now - (i as i64) },
                );
            }
            assert_eq!(store.len(), MAX_STICKY_SESSIONS);
        }
        prune_sticky_combo_sessions();
        {
            let store = store();
            assert_eq!(store.len(), MAX_STICKY_SESSIONS - MAX_STICKY_SESSIONS / 4);
            // The oldest entries (largest index) are the ones dropped.
            assert!(!store.contains_key(&format!("smart:s{}", MAX_STICKY_SESSIONS - 1)));
            assert!(!store.contains_key(&format!("smart:s{}", MAX_STICKY_SESSIONS - MAX_STICKY_SESSIONS / 4)));
            assert!(store.contains_key("smart:s0"));
            assert!(store.contains_key(&format!("smart:s{}", MAX_STICKY_SESSIONS / 4 - 1)));
        }
        reset_sticky_combo_store();
    }

    #[test]
    fn build_route_request_shape() {
        let req = build_route_request("hello", &models(&["oc/a"]));
        assert_eq!(req["state"], Json::String("hello".into()));
        assert_eq!(req["criteria"], json!({ "oc/a": null }));
    }
}
