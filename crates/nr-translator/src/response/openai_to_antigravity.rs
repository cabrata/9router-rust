//! OpenAI SSE chunk → Antigravity SSE response translator.
//! Port of open-sse/translator/response/openai-to-antigravity.js.

use nr_core::Json;
use serde_json::json;

use crate::concerns::reasoning;
use crate::schema::{gemini_finish, gemini_roles, openai_finish};
use crate::state::StreamState;

/// Convert an OpenAI chunk → Antigravity-shaped chunk.
pub fn openai_to_antigravity_response(chunk: Option<&Json>, state: &mut StreamState) -> Vec<Json> {
    let Some(chunk) = chunk else { return Vec::new() };

    let Some(choice) = chunk.get("choices").and_then(|c| c.as_array()).and_then(|c| c.first()) else {
        if let Some(u) = chunk.get("usage") {
            state.usage = Some(u.clone());
        }
        return Vec::new();
    };

    let delta = choice.get("delta").cloned().unwrap_or_else(|| json!({}));
    let finish_reason = choice.get("finish_reason").and_then(|v| v.as_str());

    if state.ext.get("toolCallAccum").is_none() {
        state.ext.insert("toolCallAccum".to_string(), json!({}));
    }
    let response_id = state
        .ext
        .get("responseId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| chunk.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()).unwrap_or_else(|| format!("resp_{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0))));
    state.ext.insert("responseId".to_string(), json!(response_id));
    let model_version = chunk.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();
    state.ext.insert("modelVersion".to_string(), json!(model_version));

    let mut parts: Vec<Json> = Vec::new();

    // Thinking/reasoning → thought part (reasoning_content + vendor aliases).
    let reasoning_text = reasoning::extract_reasoning_text(&delta);
    if !reasoning_text.is_empty() {
        parts.push(json!({"thought": true, "text": reasoning_text}));
    }

    if let Some(content) = delta.get("content").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        parts.push(json!({"text": content}));
    }

    // Accumulate tool calls silently (emit once at finish).
    if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tool_calls {
            let idx = tc.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
            let accum = state.ext.get("toolCallAccum").cloned().unwrap_or_else(|| json!({}));
            let entry = accum.get(&idx.to_string()).cloned().unwrap_or_else(|| json!({"id": "", "name": "", "arguments": ""}));
            let mut entry = entry;
            if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                entry["id"] = json!(id);
            }
            if let Some(name) = tc.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str()) {
                let existing = entry.get("name").and_then(|v| v.as_str()).unwrap_or("");
                entry["name"] = json!(format!("{existing}{name}"));
            }
            if let Some(args) = tc.get("function").and_then(|f| f.get("arguments")).and_then(|v| v.as_str()) {
                let existing = entry.get("arguments").and_then(|v| v.as_str()).unwrap_or("");
                entry["arguments"] = json!(format!("{existing}{args}"));
            }
            if let Some(obj) = state.ext.get_mut("toolCallAccum").and_then(|m| m.as_object_mut()) {
                obj.insert(idx.to_string(), entry);
            }
        }
        if parts.is_empty() && finish_reason.is_none() {
            return Vec::new();
        }
    }

    if let Some(finish_reason) = finish_reason {
        let accum = state.ext.get("toolCallAccum").and_then(|m| m.as_object()).cloned().unwrap_or_default();
        let mut indices: Vec<_> = accum.keys().collect();
        indices.sort();
        for idx in indices {
            let entry = accum.get(idx).cloned().unwrap_or_else(|| json!({}));
            let arguments = entry.get("arguments").and_then(|v| v.as_str()).unwrap_or("");
            let args: Json = serde_json::from_str(arguments).unwrap_or_else(|_| json!({}));
            let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("");
            // Restore original tool name if it was prefixed during cloaking.
            let original_name = state
                .tool_name_map
                .as_ref()
                .and_then(|m| m.get(name))
                .cloned()
                .unwrap_or_else(|| name.to_string());
            parts.push(json!({"functionCall": {"name": original_name, "args": args}}));
        }
    }

    if parts.is_empty() && finish_reason.is_none() {
        return Vec::new();
    }
    if parts.is_empty() && finish_reason.is_some() {
        parts.push(json!({"text": ""}));
    }

    let mut candidate = json!({"content": {"role": gemini_roles::MODEL, "parts": parts}});
    if let Some(finish_reason) = finish_reason {
        let gemini_reason = match finish_reason {
            openai_finish::STOP => gemini_finish::STOP,
            openai_finish::LENGTH => gemini_finish::MAX_TOKENS,
            openai_finish::TOOL_CALLS => gemini_finish::STOP,
            openai_finish::CONTENT_FILTER => gemini_finish::SAFETY,
            _ => gemini_finish::STOP,
        };
        candidate["finishReason"] = json!(gemini_reason);
    }

    let response_id = state.ext.get("responseId").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let model_version = state.ext.get("modelVersion").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mut response = json!({
        "candidates": [candidate],
        "modelVersion": model_version,
        "responseId": response_id
    });

    let usage = chunk.get("usage").or(state.usage.as_ref()).cloned();
    if let Some(u) = usage.filter(|v| v.is_object()) {
        let mut meta = json!({
            "promptTokenCount": u.get("prompt_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
            "candidatesTokenCount": u.get("completion_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
            "totalTokenCount": u.get("total_tokens").and_then(|v| v.as_i64()).unwrap_or(0)
        });
        if let Some(reasoning_tokens) = u.get("completion_tokens_details").and_then(|d| d.get("reasoning_tokens")).and_then(|v| v.as_i64()) {
            meta["thoughtsTokenCount"] = json!(reasoning_tokens);
        }
        if let Some(cached_tokens) = u.get("prompt_tokens_details").and_then(|d| d.get("cached_tokens")).and_then(|v| v.as_i64()) {
            meta["cachedContentTokenCount"] = json!(cached_tokens);
        }
        response["usageMetadata"] = meta;
    }

    vec![json!({"response": response})]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_text_chunk_maps_to_thought() {
        let mut state = StreamState::default();
        let chunk = json!({
            "id": "resp_1",
            "model": "gemini-2.5-pro",
            "choices": [{"delta": {"content": "hello"}, "finish_reason": null}]
        });
        let out = openai_to_antigravity_response(Some(&chunk), &mut state);
        assert_eq!(out.len(), 1);
        let candidate = &out[0]["response"]["candidates"][0];
        assert_eq!(candidate["content"]["role"], "model");
        assert_eq!(candidate["content"]["parts"][0]["text"], "hello");
        assert!(candidate.get("finishReason").is_none());
        assert_eq!(out[0]["response"]["responseId"], "resp_1");
    }

    #[test]
    fn test_reasoning_content_becomes_thought_part() {
        let mut state = StreamState::default();
        let chunk = json!({
            "choices": [{"delta": {"reasoning_content": "thinking"}, "finish_reason": null}]
        });
        let out = openai_to_antigravity_response(Some(&chunk), &mut state);
        let parts = out[0]["response"]["candidates"][0]["content"]["parts"].as_array().unwrap();
        assert_eq!(parts[0]["thought"], true);
        assert_eq!(parts[0]["text"], "thinking");
    }

    #[test]
    fn test_tool_calls_emit_only_at_finish() {
        let mut state = StreamState::default();
        let args_str = "{\"q\": \"v\"}";
        let call_chunk = json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "c1", "function": {"name": "search", "arguments": args_str}}
            ]}}]
        });
        let out = openai_to_antigravity_response(Some(&call_chunk), &mut state);
        assert!(out.is_empty());

        let finish_chunk = json!({
            "choices": [{"delta": {}, "finish_reason": "tool_calls"}]
        });
        let out = openai_to_antigravity_response(Some(&finish_chunk), &mut state);
        let parts = out[0]["response"]["candidates"][0]["content"]["parts"].as_array().unwrap();
        assert_eq!(parts[0]["functionCall"]["name"], "search");
        assert_eq!(parts[0]["functionCall"]["args"]["q"], "v");
        assert_eq!(out[0]["response"]["candidates"][0]["finishReason"], "STOP");
    }

    #[test]
    fn test_tool_name_map_restores_client_name() {
        let mut state = StreamState::default();
        state.tool_name_map = Some(
            [("search__x".to_string(), "search".to_string())]
                .into_iter()
                .collect::<std::collections::HashMap<_, _>>(),
        );
        let call_chunk = json!({
            "choices": [{
                "delta": {"tool_calls": [{"index": 0, "id": "c1", "function": {"name": "search__x", "arguments": "{}"}}]},
                "finish_reason": "tool_calls"
            }]
        });
        let _ = openai_to_antigravity_response(Some(&call_chunk), &mut state);
        let finish_chunk = json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]});
        let out = openai_to_antigravity_response(Some(&finish_chunk), &mut state);
        assert_eq!(out[0]["response"]["candidates"][0]["content"]["parts"][0]["functionCall"]["name"], "search");
    }

    #[test]
    fn test_usage_metadata_carries_counts() {
        let mut state = StreamState::default();
        let chunk = json!({
            "choices": [{"delta": {}, "finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15,
                "completion_tokens_details": {"reasoning_tokens": 2},
                "prompt_tokens_details": {"cached_tokens": 3}
            }
        });
        let out = openai_to_antigravity_response(Some(&chunk), &mut state);
        let meta = &out[0]["response"]["usageMetadata"];
        assert_eq!(meta["promptTokenCount"], 10);
        assert_eq!(meta["candidatesTokenCount"], 5);
        assert_eq!(meta["totalTokenCount"], 15);
        assert_eq!(meta["thoughtsTokenCount"], 2);
        assert_eq!(meta["cachedContentTokenCount"], 3);
    }
}
