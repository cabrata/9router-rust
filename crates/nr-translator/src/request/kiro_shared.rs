//! Kiro request helpers shared by the openai→kiro and claude→kiro translators.

use nr_core::Json;
use serde_json::json;

/// Process-wide replay store, mirroring the module-level Map in kiroSessionReplay.js.
pub fn replay_store() -> &'static std::sync::Mutex<crate::concerns::kiro_session_replay::KiroSessionReplayStore> {
    static STORE: once_cell::sync::Lazy<std::sync::Mutex<crate::concerns::kiro_session_replay::KiroSessionReplayStore>> =
        once_cell::sync::Lazy::new(|| std::sync::Mutex::new(crate::concerns::kiro_session_replay::KiroSessionReplayStore::new()));
    &STORE
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
    let resolved = nr_data::kiro_constants::resolve_kiro_model_intent(model);
    KiroModelIntent {
        model: resolved
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or(model)
            .to_string(),
        upstream: resolved
            .get("upstream")
            .and_then(|v| v.as_str())
            .unwrap_or(model)
            .to_string(),
        agentic: resolved.get("agentic").and_then(|v| v.as_bool()).unwrap_or(false),
        thinking_override: resolved.get("thinkingOverride").filter(|v| !v.is_null()).cloned(),
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
    // `nr-data` owns the canonical implementation; the header map is reshaped into
    // the JSON object it expects (keys keep their original case there).
    let headers_json = headers
        .map(|h| Json::Object(h.clone()))
        .unwrap_or(Json::Null);
    nr_data::kiro_constants::resolve_kiro_thinking_budget(body, Some(&headers_json), model)
        .map(|budget| budget as i64)
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
    use crate::concerns::thinking::effort_to_budget;

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
