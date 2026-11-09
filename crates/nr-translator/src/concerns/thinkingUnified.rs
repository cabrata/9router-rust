//! Unified thinking normalization: extract client intent → apply provider-native format.
//! Port of open-sse/translator/concerns/thinkingUnified.js.
//! TODO(dedupe): full implementation with thinkingFormat resolution and application

use nr_core::Json;
use crate::concerns::thinking::{budget_to_level, effort_to_budget};

/// Extract unified thinking intent from a request body (post-translation, mixed shapes).
/// Returns { mode, budget?, level? } or null when no thinking intent present.
/// TODO: implement full extraction logic for all formats (claude, openai, gemini, qwen, etc.)
pub fn extract_thinking(body: &Json) -> Option<Json> {
    if !body.is_object() {
        return None;
    }

    // Claude output_config.effort (explicit) — priority over adaptive thinking
    if let Some(oc) = body.get("output_config").and_then(|v| v.get("effort")).and_then(|v| v.as_str()) {
        if !oc.is_empty() {
            let e = oc.to_lowercase();
            return Some(if e == "none" || e == "off" {
                serde_json::json!({"mode": "none"})
            } else if e == "auto" {
                serde_json::json!({"mode": "auto"})
            } else {
                serde_json::json!({"mode": "level", "level": e})
            });
        }
    }

    // OpenAI shape
    if let Some(effort) = body.get("reasoning_effort").and_then(|v| v.as_str()) {
        if !effort.is_empty() {
            let e = effort.to_lowercase();
            return Some(if e == "none" || e == "off" {
                serde_json::json!({"mode": "none"})
            } else if e == "auto" {
                serde_json::json!({"mode": "auto"})
            } else {
                serde_json::json!({"mode": "level", "level": e})
            });
        }
    }

    // Claude thinking config
    if let Some(t) = body.get("thinking").and_then(|v| v.as_object()) {
        if let Some(tt) = t.get("type").and_then(|v| v.as_str()) {
            if tt == "disabled" {
                return Some(serde_json::json!({"mode": "none"}));
            }
            if tt == "adaptive" || tt == "enabled" {
                if let Some(budget) = t.get("budget_tokens").and_then(|v| v.as_i64()) {
                    if budget > 0 {
                        return Some(serde_json::json!({"mode": "budget", "budget": budget}));
                    }
                }
                return Some(serde_json::json!({"mode": "auto"}));
            }
        }
    }

    None
}

/// Capture thinking intent from a body before format translation strips it.
/// TODO: full implementation with display field for OpenAI thinking intent
pub fn capture_thinking(body: &Json) -> Option<Json> {
    extract_thinking(body)
}

/// Apply thinking config to the target format.
/// TODO(dedupe): full implementation with provider-specific format conversion
pub fn apply_thinking(_target_format: &str, _model: &str, _body: &mut Json, _provider: Option<&str>, _thinking_intent: Option<&Json>) {
    // Stub: full implementation needed
}

/// Strip thinking suffix "model(value)" → "model" (no-op when absent).
pub fn strip_thinking_suffix(model: &str) -> &str {
    if let Some(pos) = model.rfind('(') {
        if let Some(close_pos) = model.rfind(')') {
            if close_pos > pos {
                return model[..pos].trim();
            }
        }
    }
    model
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_thinking_openai() {
        let body = serde_json::json!({"reasoning_effort": "high"});
        let result = extract_thinking(&body);
        assert!(result.is_some());
        assert_eq!(result.unwrap().get("level").and_then(|v| v.as_str()), Some("high"));
    }

    #[test]
    fn test_extract_thinking_claude() {
        let body = serde_json::json!({
            "thinking": {
                "type": "enabled",
                "budget_tokens": 8192
            }
        });
        let result = extract_thinking(&body);
        assert!(result.is_some());
        assert_eq!(result.unwrap().get("budget").and_then(|v| v.as_i64()), Some(8192));
    }
}
