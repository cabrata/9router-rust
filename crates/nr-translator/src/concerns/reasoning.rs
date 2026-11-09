//! Reasoning content handling. Port of open-sse/translator/concerns/reasoning.js.

use nr_core::Json;
use serde_json::json;
use crate::schema::roles;

/// Build OpenAI delta carrying reasoning_content (optional leading assistant role)
pub fn reasoning_delta(text: &str, with_role: bool) -> Json {
    if with_role {
        json!({
            "role": roles::ASSISTANT,
            "reasoning_content": text
        })
    } else {
        json!({
            "reasoning_content": text
        })
    }
}

/// Lazily add the canonical OpenAI reasoning field when an upstream streams the
/// vendor aliases instead: `reasoning` (string) and `reasoning_details[]`
/// (OpenRouter-style lanes). Only ever ADDS the field — the aliases stay intact
/// for clients that read them — and never touches a delta that already carries
/// reasoning_content. Without this, OpenAI-shaped clients see no thinking at all:
/// reasoningDelta()/the SSE→JSON aggregator/usage tracking all read
/// reasoning_content.
pub fn lift_reasoning_aliases(delta: &Json) -> Json {
    if !delta.is_object() {
        return delta.clone();
    }

    if delta.get("reasoning_content").and_then(|v| v.as_str()).is_some() {
        return delta.clone();
    }

    let text = extract_reasoning_text(delta);
    if !text.is_empty() {
        let mut result = delta.clone();
        result.as_object_mut().unwrap().insert("reasoning_content".to_string(), json!(text));
        result
    } else {
        delta.clone()
    }
}

/// Extract reasoning text from a streamed OpenAI-compatible delta across vendor shapes:
///   - reasoning_content (GLM, Qwen, DeepSeek, Kimi, Step, Hunyuan)
///   - reasoning (some compat layers)
///   - reasoning_details[] (MiniMax reasoning_split=true): [{ text|content }]
/// Returns concatenated reasoning string, or "" when none.
pub fn extract_reasoning_text(delta: &Json) -> String {
    if !delta.is_object() {
        return String::new();
    }

    if let Some(rc) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
        if !rc.is_empty() {
            return rc.to_string();
        }
    }

    if let Some(r) = delta.get("reasoning").and_then(|v| v.as_str()) {
        if !r.is_empty() {
            return r.to_string();
        }
    }

    if let Some(details) = delta.get("reasoning_details").and_then(|v| v.as_array()) {
        return details
            .iter()
            .map(|d| {
                if let Some(s) = d.as_str() {
                    s.to_string()
                } else if let Some(obj) = d.as_object() {
                    obj.get("text")
                        .or_else(|| obj.get("content"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string()
                } else {
                    String::new()
                }
            })
            .collect::<String>();
    }

    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reasoning_delta_without_role() {
        let delta = reasoning_delta("thinking...", false);
        assert_eq!(delta.get("reasoning_content").and_then(|v| v.as_str()), Some("thinking..."));
        assert!(delta.get("role").is_none());
    }

    #[test]
    fn test_reasoning_delta_with_role() {
        let delta = reasoning_delta("thinking...", true);
        assert_eq!(delta.get("reasoning_content").and_then(|v| v.as_str()), Some("thinking..."));
        assert_eq!(delta.get("role").and_then(|v| v.as_str()), Some(roles::ASSISTANT));
    }

    #[test]
    fn test_extract_reasoning_content() {
        let delta = json!({"reasoning_content": "test"});
        assert_eq!(extract_reasoning_text(&delta), "test");
    }

    #[test]
    fn test_extract_reasoning_alias() {
        let delta = json!({"reasoning": "test"});
        assert_eq!(extract_reasoning_text(&delta), "test");
    }

    #[test]
    fn test_extract_reasoning_details() {
        let delta = json!({
            "reasoning_details": [
                {"text": "part1"},
                {"text": "part2"}
            ]
        });
        assert_eq!(extract_reasoning_text(&delta), "part1part2");
    }

    #[test]
    fn test_lift_reasoning_aliases() {
        let delta = json!({"reasoning": "test"});
        let lifted = lift_reasoning_aliases(&delta);
        assert_eq!(lifted.get("reasoning_content").and_then(|v| v.as_str()), Some("test"));
        assert_eq!(lifted.get("reasoning").and_then(|v| v.as_str()), Some("test"));
    }
}
