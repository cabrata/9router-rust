//! Kiro request helpers shared by the openai→kiro and claude→kiro translators.
//!
//! TODO(dedupe): drop this module once nr-data's `resolve_kiro_model_intent` and
//! `resolve_kiro_thinking_budget` stop being placeholders (they currently ignore
//! the `model(level)` suffix and always return a 16000 budget).

use nr_core::Json;
use serde_json::json;

use crate::concerns::thinking::effort_to_budget;
use crate::concerns::thinkingUnified::{extract_thinking, parse_suffix};

/// Process-wide replay store, mirroring the module-level Map in kiroSessionReplay.js.
pub fn replay_store() -> &'static std::sync::Mutex<crate::concerns::kiro_session_replay::KiroSessionReplayStore> {
    static STORE: once_cell::sync::Lazy<std::sync::Mutex<crate::concerns::kiro_session_replay::KiroSessionReplayStore>> =
        once_cell::sync::Lazy::new(|| std::sync::Mutex::new(crate::concerns::kiro_session_replay::KiroSessionReplayStore::new()));
    &STORE
}

/// KIRO_THINKING_BUDGET_DEFAULT from open-sse/config/kiroConstants.js.
fn kiro_thinking_budget_default() -> i64 {
    nr_data::export("open-sse/config/kiroConstants", "KIRO_THINKING_BUDGET_DEFAULT")
        .and_then(|v| v.as_i64())
        .unwrap_or(16000)
}

/// `resolveKiroModelIntent(model)` from kiroConstants.js.
pub(crate) struct KiroModelIntent {
    /// Model id after the generic `model(level)` suffix was consumed.
    pub model: String,
    pub upstream: String,
    pub agentic: bool,
    pub thinking_override: Option<Json>,
}

pub(crate) fn resolve_model_intent(model: &str) -> KiroModelIntent {
    let (clean_model, override_suffix) = parse_suffix(model);
    let resolved = nr_data::kiro_constants::resolve_kiro_model(&clean_model);
    KiroModelIntent {
        model: clean_model.clone(),
        upstream: resolved
            .get("upstream")
            .and_then(|v| v.as_str())
            .unwrap_or(&clean_model)
            .to_string(),
        agentic: resolved.get("agentic").and_then(|v| v.as_bool()).unwrap_or(false),
        thinking_override: override_suffix,
    }
}

/// `applyKiroThinkingOverride(body, override)` from kiroConstants.js.
pub(crate) fn apply_thinking_override(body: &Json, override_suffix: Option<&Json>) -> Json {
    nr_data::kiro_constants::apply_kiro_thinking_override(body, override_suffix)
}

/// `resolveKiroThinkingBudget(body, headers, model)` from kiroConstants.js.
pub(crate) fn resolve_thinking_budget(
    body: Option<&Json>,
    headers: Option<&serde_json::Map<String, Json>>,
    model: Option<&str>,
) -> Option<i64> {
    let default = kiro_thinking_budget_default();
    if let Some(cfg) = body.and_then(extract_thinking) {
        match cfg.get("mode").and_then(|v| v.as_str()).unwrap_or("") {
            "none" => return None,
            "level" => {
                let level = cfg.get("level").and_then(|v| v.as_str()).unwrap_or("");
                if level == "disabled" {
                    return None;
                }
                return Some(effort_to_budget(level).unwrap_or(default));
            }
            "budget" => return cfg.get("budget").and_then(|v| v.as_i64()),
            _ => return Some(default),
        }
    }

    // Anthropic beta header switching on interleaved thinking.
    if let Some(headers) = headers {
        let beta = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("anthropic-beta"))
            .and_then(|(_, v)| v.as_str());
        if beta.map(|b| b.to_lowercase().contains("interleaved-thinking")).unwrap_or(false) {
            return Some(default);
        }
    }

    if body.map(contains_thinking_mode_tag).unwrap_or(false) {
        return Some(default);
    }

    if let Some(model) = model.filter(|m| !m.is_empty()) {
        let m = model.to_lowercase();
        if m.contains("thinking") || m.contains("-reason") {
            return Some(default);
        }
    }
    None
}

fn contains_thinking_mode_tag(body: &Json) -> bool {
    let messages = body.get("messages").and_then(|v| v.as_array());
    if let Some(messages) = messages {
        for msg in messages {
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
            if role != "system" && role != "user" {
                continue;
            }
            match msg.get("content") {
                Some(Json::String(text)) if contains_tag_in_text(text) => return true,
                Some(Json::Array(parts)) => {
                    for part in parts {
                        if part.get("text").and_then(|v| v.as_str()).map(contains_tag_in_text).unwrap_or(false) {
                            return true;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    body.get("system").and_then(|v| v.as_str()).map(contains_tag_in_text).unwrap_or(false)
}

fn contains_tag_in_text(text: &str) -> bool {
    if !text.contains("<thinking_mode>") {
        return false;
    }
    text.contains("<thinking_mode>enabled</thinking_mode>") || text.contains("<thinking_mode>interleaved</thinking_mode>")
}

/// `profileArn` resolution shared by both request translators.
/// api_key/idc/external_idp carry an account-specific (or token-bound) profile;
/// the shared builder-id/social default ARN belongs to another account and makes
/// CodeWhisperer answer 403 "bearer token invalid".
pub(crate) fn resolve_profile_arn(psd: Option<&serde_json::Map<String, Json>>) -> String {
    let auth_method = psd.and_then(|p| p.get("authMethod")).and_then(|v| v.as_str());
    let resolved = psd.and_then(|p| p.get("profileArn")).and_then(|v| v.as_str()).map(|s| s.to_string());
    let account_bound = matches!(auth_method, Some("api_key") | Some("idc") | Some("external_idp"));
    if account_bound {
        resolved.unwrap_or_default()
    } else {
        resolved.unwrap_or_else(|| nr_data::kiro_constants::resolve_default_profile_arn(auth_method).to_string())
    }
}

/// Tool-name reverse map for `_toolNameMap` (sanitized → client name).
pub(crate) fn restored_tool_names(name_map: &std::collections::HashMap<String, String>) -> serde_json::Map<String, Json> {
    let mut restored = serde_json::Map::new();
    for (original, sanitized) in name_map {
        if original != sanitized {
            restored.insert(sanitized.clone(), json!(original));
        }
    }
    restored
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_model_intent_strips_agentic() {
        let intent = resolve_model_intent("kiro-claude-sonnet-4-agentic");
        assert!(intent.agentic);
        assert!(!intent.upstream.contains("agentic"));
    }

    #[test]
    fn test_resolve_thinking_budget_none_without_intent() {
        assert_eq!(resolve_thinking_budget(Some(&json!({"messages": []})), None, Some("gpt-4o-mini")), None);
    }

    #[test]
    fn test_resolve_thinking_budget_from_claude_thinking() {
        let body = json!({"thinking": {"type": "enabled", "budget_tokens": 8000}});
        assert_eq!(resolve_thinking_budget(Some(&body), None, None), Some(8000));
    }

    #[test]
    fn test_resolve_thinking_budget_disabled() {
        let body = json!({"thinking": {"type": "disabled"}});
        assert_eq!(resolve_thinking_budget(Some(&body), None, None), None);
    }

    #[test]
    fn test_resolve_thinking_budget_from_level() {
        let body = json!({"reasoning_effort": "medium"});
        assert_eq!(resolve_thinking_budget(Some(&body), None, None), effort_to_budget("medium"));
    }

    #[test]
    fn test_resolve_thinking_budget_from_model_hint() {
        assert_eq!(resolve_thinking_budget(None, None, Some("claude-sonnet-4-thinking")), Some(16000));
    }

    #[test]
    fn test_resolve_thinking_budget_from_tag() {
        let body = json!({"messages": [{"role": "user", "content": "<thinking_mode>enabled</thinking_mode>hi"}]});
        assert_eq!(resolve_thinking_budget(Some(&body), None, None), Some(16000));
    }

    #[test]
    fn test_resolve_thinking_budget_from_beta_header() {
        let mut headers = serde_json::Map::new();
        headers.insert("Anthropic-Beta".into(), json!("interleaved-thinking-2025-05-14"));
        assert_eq!(resolve_thinking_budget(Some(&json!({})), Some(&headers), None), Some(16000));
    }

    #[test]
    fn test_resolve_profile_arn_account_bound_has_no_default() {
        let mut psd = serde_json::Map::new();
        psd.insert("authMethod".into(), json!("api_key"));
        assert_eq!(resolve_profile_arn(Some(&psd)), "");
    }

    #[test]
    fn test_resolve_profile_arn_oauth_falls_back_to_default() {
        let mut psd = serde_json::Map::new();
        psd.insert("authMethod".into(), json!("oauth"));
        assert!(!resolve_profile_arn(Some(&psd)).is_empty());
    }
}
