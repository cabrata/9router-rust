//! Kiro → Claude response translator (DIRECT route, no OpenAI pivot).
//! Port of open-sse/translator/response/kiro-to-claude.js.
//!
//! KiroExecutor already emits OpenAI-shaped `chat.completion.chunk` objects.
//! Chunks arriving here are OpenAI streaming chunks converted into Claude SSE events.

use nr_core::Json;
use serde_json::json;

use crate::response::openai_to_claude::openai_to_claude_response;
use crate::state::StreamState;

fn restore_tool_name(state: &StreamState, name: &str) -> String {
    state
        .tool_name_map
        .as_ref()
        .and_then(|m| m.get(name))
        .cloned()
        .unwrap_or_else(|| name.to_string())
}

/// Convert one OpenAI-format chunk (from KiroExecutor) into Claude SSE events.
pub fn kiro_to_claude_response(chunk: Option<&Json>, state: &mut StreamState) -> Vec<Json> {
    let Some(chunk) = chunk else {
        return openai_to_claude_response(None, state);
    };

    // If chunk is a raw JSON string, parse it defensively.
    let data: Json = if let Some(s) = chunk.as_str() {
        let trimmed = s.trim();
        if trimmed.is_empty() || trimmed == "[DONE]" {
            return Vec::new();
        }
        let stripped = trimmed.strip_prefix("data:").map(|l| l.trim()).unwrap_or(trimmed);
        match serde_json::from_str(stripped) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        }
    } else {
        chunk.clone()
    };

    // Restore tool name in chunk if present.
    let mut modified = data;
    if let Some(choices) = modified.get_mut("choices").and_then(|c| c.as_array_mut()) {
        for choice in choices.iter_mut() {
            if let Some(tool_calls) = choice.get_mut("delta").and_then(|d| d.get_mut("tool_calls")).and_then(|t| t.as_array_mut()) {
                for tc in tool_calls.iter_mut() {
                    if let Some(name) = tc.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str()) {
                        let restored = restore_tool_name(state, name);
                        tc["function"]["name"] = json!(restored);
                    }
                }
            }
        }
    }

    openai_to_claude_response(Some(&modified), state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kiro_to_claude_converts_text_chunk() {
        let mut state = StreamState::default();
        let chunk = json!({
            "id": "chatcmpl-1",
            "choices": [{"index": 0, "delta": {"content": "hello"}, "finish_reason": null}]
        });
        let events = kiro_to_claude_response(Some(&chunk), &mut state);
        assert!(!events.is_empty());
        assert!(events.iter().any(|e| e.get("type").and_then(|v| v.as_str()) == Some("content_block_delta")));
    }

    #[test]
    fn test_kiro_to_claude_restores_tool_name() {
        let mut state = StreamState::default();
        state.tool_name_map = Some([("f__x".to_string(), "f".to_string())].into_iter().collect());
        let chunk = json!({
            "id": "chatcmpl-1",
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{"index": 0, "id": "t1", "function": {"name": "f__x", "arguments": "{}"}}]
                },
                "finish_reason": null
            }]
        });
        let events = kiro_to_claude_response(Some(&chunk), &mut state);
        let start_event = events.iter().find(|e| e.get("type").and_then(|v| v.as_str()) == Some("content_block_start"));
        if let Some(e) = start_event {
            let name = e.get("content_block").and_then(|cb| cb.get("name")).and_then(|v| v.as_str()).unwrap_or("");
            assert_eq!(name, "f");
        }
    }
}
