//! Port of open-sse/services/capacityAdapter.js
//!
//! Capacity Adapter — global fallback pools of models per input-modality
//! capability (vision / pdf / audioInput / videoInput).
//!
//! The pool models are appended as extra fallback candidates behind whatever
//! models were already going to be tried (a combo's members, or a single target
//! model). combo.js's existing reorderByCapabilities then floats a capable pool
//! model to the front only when none of the original models can handle the
//! request — so this never overrides a combo that already has a member covering
//! the capability.

use nr_core::Json;
use serde_json::json;

const CAPABILITY_KEYS: &[&str] = &["vision", "pdf", "audioInput", "videoInput"];
const DEFAULT_FALLBACK_MODEL: &str = "oc/mimo-v2.6-flash-free";
const LEGACY_MODEL: &str = "oc/mimo-v2.5-free";

fn upgrade_legacy_model(m: &str) -> String {
    if m == LEGACY_MODEL {
        DEFAULT_FALLBACK_MODEL.to_string()
    } else {
        m.to_string()
    }
}

fn is_hard_cap(cap: &str) -> bool {
    CAPABILITY_KEYS.contains(&cap)
}

/// One capability's normalized config: `{ enabled, roundRobin, models }`.
#[derive(Debug, Clone, PartialEq)]
pub struct CapEntry {
    pub enabled: bool,
    pub round_robin: bool,
    pub models: Vec<String>,
}

/// Normalize a capability entry. Backward-compat: accept the legacy array form
/// `[{model, enabled}]` (treated as enabled, fallback).
fn normalize_cap_entry(entry: Option<&Json>) -> CapEntry {
    match entry {
        Some(Json::Array(items)) => CapEntry {
            enabled: true,
            round_robin: false,
            models: items
                .iter()
                .map(|e| {
                    let raw = match e {
                        Json::String(s) => s.clone(),
                        Json::Object(_) => {
                            e.get("model").and_then(Json::as_str).unwrap_or("").to_string()
                        }
                        _ => String::new(),
                    };
                    if raw.is_empty() { None } else { Some(upgrade_legacy_model(&raw)) }
                })
                .flatten()
                .collect(),
        },
        Some(obj @ Json::Object(_)) => {
            // `entry.enabled !== false`
            let enabled = obj.get("enabled") != Some(&Json::Bool(false));
            let round_robin = nr_core::json::truthy(obj.get("roundRobin").unwrap_or(&Json::Null));
            let models = obj
                .get("models")
                .and_then(Json::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Json::as_str)
                        .filter(|s| !s.is_empty())
                        .map(upgrade_legacy_model)
                        .collect()
                })
                .unwrap_or_default();
            CapEntry { enabled, round_robin, models }
        }
        _ => CapEntry { enabled: false, round_robin: false, models: Vec::new() },
    }
}

/// Resolve one capability's full config. Enabled pools with no models fall back
/// to `DEFAULT_FALLBACK_MODEL` so the toggle is never a no-op.
pub fn get_capacity_adapter_config(cap: &str, settings: &Json) -> CapEntry {
    let entry = normalize_cap_entry(settings.get("capacityAdapter").and_then(|c| c.get(cap)));
    if entry.enabled && entry.models.is_empty() {
        return CapEntry { models: vec![DEFAULT_FALLBACK_MODEL.to_string()], ..entry };
    }
    entry
}

/// Flatten enabled models across all capability pools, in priority order, deduped.
pub fn get_capacity_adapter_models(settings: &Json) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut models = Vec::new();
    for cap in CAPABILITY_KEYS {
        let entry = get_capacity_adapter_config(cap, settings);
        if !entry.enabled {
            continue;
        }
        for m in entry.models {
            if seen.insert(m.clone()) {
                models.push(m);
            }
        }
    }
    models
}

/// Strategy for a capability: "round-robin" when enabled+roundRobin, else "fallback".
pub fn get_capacity_adapter_strategy(cap: &str, settings: &Json) -> &'static str {
    let entry = get_capacity_adapter_config(cap, settings);
    if entry.enabled && entry.round_robin { "round-robin" } else { "fallback" }
}

/// Strategy from the request's required capabilities: picks the first capability
/// whose adapter pool is enabled and can satisfy a hard requirement.
pub fn get_active_adapter_strategy(required_capabilities: &[String], settings: &Json) -> &'static str {
    for cap in required_capabilities.iter().filter(|c| is_hard_cap(c)) {
        let entry = get_capacity_adapter_config(cap, settings);
        if !entry.enabled || entry.models.is_empty() {
            continue;
        }
        return get_capacity_adapter_strategy(cap, settings);
    }
    "fallback"
}

/// Split `provider/model` the way the JS `indexOf("/") > 0` does.
fn split_model(model_str: &str) -> (String, String) {
    match model_str.find('/') {
        Some(idx) if idx > 0 => (model_str[..idx].to_string(), model_str[idx + 1..].to_string()),
        _ => (String::new(), model_str.to_string()),
    }
}

fn model_satisfies(model_str: &str, required_hard: &[String]) -> bool {
    let (provider, model) = split_model(model_str);
    let caps = nr_data::capabilities::get_capabilities_for_model(
        if provider.is_empty() { None } else { Some(&provider) },
        Some(&model),
    );
    required_hard
        .iter()
        .all(|c| caps.get(c.as_str()) == Some(&Json::Bool(true)))
}

/// Prepend capacity-adapter models as priority candidates when NONE of the
/// original models (combo members, or the single target model) can satisfy the
/// request's required capabilities. Adapter models go FIRST (priority); the
/// original models follow as fallback. Leaves `models` untouched when the
/// original list already covers it (combo.js's reorderByCapabilities handles
/// that case via autoSwitch).
pub fn augment_models_with_capacity_adapter(
    models: Vec<String>,
    required_capabilities: &[String],
    settings: &Json,
) -> Vec<String> {
    let hard: Vec<String> = required_capabilities
        .iter()
        .filter(|c| is_hard_cap(c))
        .cloned()
        .collect();
    if hard.is_empty() || models.is_empty() {
        return models;
    }
    if models.iter().any(|m| model_satisfies(m, &hard)) {
        return models;
    }

    let pool: Vec<String> = get_capacity_adapter_models(settings)
        .into_iter()
        .filter(|m| !models.contains(m) && model_satisfies(m, &hard))
        .collect();
    if pool.is_empty() {
        return models;
    }
    let mut out = pool;
    out.extend(models);
    out
}

const CHARS_PER_TOKEN: f64 = 4.0; // rough estimate; avoids a tokenizer dependency
const HEAD_KEEP: usize = 6; // messages after system kept verbatim before dropping the middle

fn block_length(content: Option<&Json>) -> usize {
    match content {
        Some(Json::String(s)) => s.chars().count(),
        Some(Json::Array(blocks)) => blocks
            .iter()
            .map(|b| {
                b.get("text")
                    .and_then(Json::as_str)
                    .map(|t| t.chars().count())
                    .unwrap_or(50)
            })
            .sum(),
        _ => 0,
    }
}

fn content_of(msg: &Json) -> Option<&Json> {
    msg.get("content").or_else(|| msg.get("parts"))
}

fn role_of(msg: &Json) -> Option<&str> {
    msg.get("role").and_then(Json::as_str)
}

fn is_system_role(role: Option<&str>) -> bool {
    matches!(role, Some("system") | Some("developer"))
}

fn is_assistant_role(role: Option<&str>) -> bool {
    matches!(role, Some("assistant") | Some("model"))
}

/// Trim history to fit a (possibly smaller) context window by dropping the MIDDLE.
/// Preserves: all system/instruction messages (head), and the trailing user run
/// carrying the media the switch happened for (tail). Older middle turns between
/// the head instructions and the current turn are dropped first.
pub fn strip_history_for_context(body: &Json, context_window: Option<f64>) -> Json {
    let key = if body.get("messages").map(Json::is_array).unwrap_or(false) {
        "messages"
    } else if body.get("input").map(Json::is_array).unwrap_or(false) {
        "input"
    } else if body.get("contents").map(Json::is_array).unwrap_or(false) {
        "contents"
    } else {
        return body.clone();
    };
    let arr = match body.get(key).and_then(Json::as_array) {
        Some(a) if !a.is_empty() => a,
        _ => return body.clone(),
    };

    let system_msgs: Vec<Json> =
        arr.iter().filter(|m| is_system_role(role_of(m))).cloned().collect();
    let rest: Vec<Json> = arr.iter().filter(|m| !is_system_role(role_of(m))).cloned().collect();
    if rest.is_empty() {
        return body.clone();
    }

    let mut i = rest.len();
    while i > 0 && !is_assistant_role(role_of(&rest[i - 1])) {
        i -= 1;
    }
    // `i` is now the index just past the last assistant message (0 when none).
    let tail: Vec<Json> = rest[i..].to_vec(); // current user turn (has media) — always kept
    let older: Vec<Json> = rest[..i].to_vec(); // everything before it
    if older.is_empty() {
        return body.clone();
    }

    // Cap at 80% of the adapter model's context window — leaves room for the response.
    let budget_chars = context_window.unwrap_or(200000.0) * 0.8 * CHARS_PER_TOKEN;

    // Prefer keeping the first HEAD_KEEP messages (initial instructions/context)
    // verbatim; only trim further if even that exceeds the adapter model's window.
    let mut head: Vec<Json> = older.iter().take(HEAD_KEEP).cloned().collect();
    let mut total: usize = system_msgs
        .iter()
        .chain(head.iter())
        .chain(tail.iter())
        .map(|m| block_length(content_of(m)))
        .sum();

    // If head + tail overflow, drop head turns from the end (closest to middle) first.
    while (total as f64) > budget_chars && !head.is_empty() {
        let dropped = head.pop();
        if let Some(dropped) = dropped {
            total -= block_length(content_of(&dropped));
        }
    }

    if head.len() == older.len() {
        return body.clone();
    }
    let merged: Vec<Json> = system_msgs.into_iter().chain(head).chain(tail).collect();
    let mut out = body.clone();
    if let Some(obj) = out.as_object_mut() {
        obj.insert(key.to_string(), Json::Array(merged));
    }
    out
}

/// Wrap a single-model handler so calls to a capacity-adapter model strip
/// history to fit its context window first. No-op passthrough when the pool is empty.
pub struct CapacityAdapterStripping {
    adapter_models: std::collections::HashSet<String>,
}

impl CapacityAdapterStripping {
    pub fn new(adapter_models: &[String]) -> Self {
        Self { adapter_models: adapter_models.iter().cloned().collect() }
    }

    /// `true` when this model is in the adapter pool (i.e. stripping applies).
    pub fn is_adapter_model(&self, model: &str) -> bool {
        self.adapter_models.contains(model)
    }

    /// Apply the context fit for an adapter model; other models pass through.
    pub fn prepare(&self, body: Json, model_str: &str) -> Json {
        if !self.adapter_models.contains(model_str) {
            return body;
        }
        let (provider, model) = split_model(model_str);
        let caps = nr_data::capabilities::get_capabilities_for_model(
            if provider.is_empty() { None } else { Some(&provider) },
            Some(&model),
        );
        let context_window = caps.get("contextWindow").and_then(Json::as_f64);
        strip_history_for_context(&body, context_window)
    }
}

/// The settings object one dashboard write produces, for reference in tests.
pub fn settings_with(cap: &str, entry: Json) -> Json {
    json!({ "capacityAdapter": { cap: entry } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn legacy_array_form_is_enabled_fallback() {
        let settings = json!({ "capacityAdapter": { "vision": [{ "model": "oc/a" }, "oc/b"] } });
        let entry = get_capacity_adapter_config("vision", &settings);
        assert!(entry.enabled);
        assert!(!entry.round_robin);
        assert_eq!(entry.models, s(&["oc/a", "oc/b"]));
        assert_eq!(get_capacity_adapter_strategy("vision", &settings), "fallback");
    }

    #[test]
    fn legacy_model_is_upgraded() {
        let settings = json!({ "capacityAdapter": { "vision": ["oc/mimo-v2.5-free"] } });
        let entry = get_capacity_adapter_config("vision", &settings);
        assert_eq!(entry.models, s(&["oc/mimo-v2.6-flash-free"]));
    }

    #[test]
    fn enabled_pool_without_models_gets_default() {
        let settings = json!({ "capacityAdapter": { "pdf": { "enabled": true, "models": [] } } });
        let entry = get_capacity_adapter_config("pdf", &settings);
        assert_eq!(entry.models, s(&["oc/mimo-v2.6-flash-free"]));
    }

    #[test]
    fn disabled_pool_stays_empty() {
        let settings = json!({ "capacityAdapter": { "pdf": { "enabled": false } } });
        let entry = get_capacity_adapter_config("pdf", &settings);
        assert!(!entry.enabled);
        assert!(entry.models.is_empty());
    }

    #[test]
    fn enabled_defaults_true_when_absent() {
        // `entry.enabled !== false` — an object without `enabled` is enabled.
        let settings = json!({ "capacityAdapter": { "vision": { "models": ["oc/a"] } } });
        assert!(get_capacity_adapter_config("vision", &settings).enabled);
    }

    #[test]
    fn missing_settings_are_disabled() {
        let entry = get_capacity_adapter_config("vision", &json!({}));
        assert!(!entry.enabled);
        assert!(entry.models.is_empty());
        assert!(!get_capacity_adapter_config("vision", &Json::Null).enabled);
    }

    #[test]
    fn round_robin_only_when_enabled_and_requested() {
        let settings = json!({ "capacityAdapter": {
            "vision": { "enabled": true, "roundRobin": true, "models": ["oc/a"] },
            "pdf": { "enabled": false, "roundRobin": true, "models": ["oc/a"] },
        }});
        assert_eq!(get_capacity_adapter_strategy("vision", &settings), "round-robin");
        assert_eq!(get_capacity_adapter_strategy("pdf", &settings), "fallback");
    }

    #[test]
    fn models_flatten_in_priority_order_deduped() {
        let settings = json!({ "capacityAdapter": {
            "vision": { "models": ["oc/a", "oc/b"] },
            "pdf": { "models": ["oc/b", "oc/c"] },
            "audioInput": { "enabled": false, "models": ["oc/d"] },
            "videoInput": { "models": ["oc/e"] },
        }});
        assert_eq!(
            get_capacity_adapter_models(&settings),
            s(&["oc/a", "oc/b", "oc/c", "oc/e"])
        );
    }

    #[test]
    fn active_strategy_uses_first_enabled_hard_cap() {
        let settings = json!({ "capacityAdapter": {
            "vision": { "enabled": true, "roundRobin": true, "models": ["oc/a"] },
            "pdf": { "enabled": true, "models": ["oc/b"] },
        }});
        assert_eq!(
            get_active_adapter_strategy(&s(&["vision", "pdf"]), &settings),
            "round-robin"
        );
        // Soft capabilities (not one of the four) never select a strategy.
        assert_eq!(
            get_active_adapter_strategy(&s(&["tools", "vision"]), &settings),
            "round-robin"
        );
        assert_eq!(get_active_adapter_strategy(&[], &settings), "fallback");
        assert_eq!(get_active_adapter_strategy(&s(&["tools"]), &settings), "fallback");
    }

    #[test]
    fn strip_history_drops_middle_and_keeps_head_and_tail() {
        let mut messages = vec![json!({ "role": "system", "content": "sys" })];
        for i in 0..10 {
            messages.push(json!({
                "role": if i % 2 == 0 { "user" } else { "assistant" },
                "content": format!("message {i}"),
            }));
        }
        messages.push(json!({ "role": "user", "content": "current" }));
        let body = json!({ "messages": messages });

        // Small window forces head trimming.
        let out = strip_history_for_context(&body, Some(20.0));
        let arr = out["messages"].as_array().expect("messages");
        assert_eq!(arr[0]["role"], Json::String("system".into()));
        // The last message (current user turn) is always kept.
        assert_eq!(
            arr.last().unwrap()["content"],
            Json::String("current".into())
        );
        assert!(arr.len() < body["messages"].as_array().unwrap().len());
    }

    #[test]
    fn strip_history_noop_when_it_fits() {
        let body = json!({ "messages": [
            { "role": "system", "content": "sys" },
            { "role": "user", "content": "hi" },
        ]});
        assert_eq!(strip_history_for_context(&body, Some(200000.0)), body);
    }

    #[test]
    fn strip_history_without_recognised_key_is_noop() {
        let body = json!({ "prompt": "hi" });
        assert_eq!(strip_history_for_context(&body, Some(10.0)), body);
    }

    #[test]
    fn strip_history_handles_input_and_contents_keys() {
        for key in ["input", "contents"] {
            let body = json!({ key: [
                { "role": "assistant", "content": "a" },
                { "role": "user", "content": "current" },
            ]});
            let out = strip_history_for_context(&body, Some(200000.0));
            assert_eq!(out[key].as_array().unwrap().len(), 2);
        }
    }

    #[test]
    fn strip_history_system_only_is_noop() {
        let body = json!({ "messages": [{ "role": "system", "content": "sys" }] });
        assert_eq!(strip_history_for_context(&body, Some(10.0)), body);
    }

    #[test]
    fn strip_history_null_window_defaults_to_200k() {
        let body = json!({ "messages": [
            { "role": "system", "content": "sys" },
            { "role": "assistant", "content": "a" },
            { "role": "user", "content": "current" },
        ]});
        // 200k * 0.8 * 4 = 640k chars: a tiny body fits.
        assert_eq!(strip_history_for_context(&body, None), body);
    }

    #[test]
    fn block_length_counts_text_parts_and_fallback() {
        assert_eq!(block_length(Some(&Json::String("abc".into()))), 3);
        assert_eq!(
            block_length(Some(&json!([
                { "type": "text", "text": "abcd" },
                { "type": "image_url" },
            ]))),
            4 + 50
        );
        assert_eq!(block_length(None), 0);
    }

    #[test]
    fn stripping_wrapper_passes_through_non_adapter_models() {
        let wrapper = CapacityAdapterStripping::new(&s(&["oc/adapter"]));
        let body = json!({ "messages": [{ "role": "user", "content": "hi" }] });
        assert!(!wrapper.is_adapter_model("oc/other"));
        assert_eq!(wrapper.prepare(body.clone(), "oc/other"), body);
    }

    #[test]
    fn stripping_wrapper_applies_to_adapter_models() {
        let wrapper = CapacityAdapterStripping::new(&s(&["oc/adapter"]));
        let body = json!({ "messages": [
            { "role": "assistant", "content": "a" },
            { "role": "user", "content": "hi" },
        ]});
        assert!(wrapper.is_adapter_model("oc/adapter"));
        // Unknown model capabilities resolve a window; the call must not panic.
        let out = wrapper.prepare(body.clone(), "oc/adapter");
        assert!(out.get("messages").is_some());
    }

    #[test]
    fn augment_is_noop_without_hard_caps() {
        let settings = json!({ "capacityAdapter": { "vision": ["oc/vision"] } });
        let models = s(&["oc/a"]);
        assert_eq!(
            augment_models_with_capacity_adapter(models.clone(), &[], &settings),
            models
        );
        assert_eq!(
            augment_models_with_capacity_adapter(models.clone(), &s(&["tools"]), &settings),
            models
        );
    }

    #[test]
    fn augment_is_noop_for_empty_models() {
        let settings = json!({ "capacityAdapter": { "vision": ["oc/vision"] } });
        assert!(augment_models_with_capacity_adapter(vec![], &s(&["vision"]), &settings).is_empty());
    }

    #[test]
    fn split_model_matches_js_index_of_rule() {
        assert_eq!(split_model("oc/a"), ("oc".to_string(), "a".to_string()));
        assert_eq!(split_model("/a"), (String::new(), "/a".to_string()));
        assert_eq!(split_model("noc"), (String::new(), "noc".to_string()));
    }

    #[test]
    fn settings_helper_shape() {
        let settings = settings_with("vision", json!({ "models": ["oc/a"] }));
        assert_eq!(settings["capacityAdapter"]["vision"]["models"][0], Json::String("oc/a".into()));
    }
}
