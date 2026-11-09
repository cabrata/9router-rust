//! CommandCode → OpenAI response translator. Port of open-sse/translator/response/commandcode-to-openai.js.
//!
//! CommandCode upstream emits NDJSON-style AI SDK v5 stream events:
//!   {"type":"text-delta"} {"type":"reasoning-delta"} {"type":"tool-input-start"} {"type":"tool-call"}
//!   {"type":"finish-step"} {"type":"finish"} etc.
//! Each upstream event arrives as one JSON object (or a JSON-line string).

use nr_core::Json;
use serde_json::json;

use crate::concerns::{chunk, finish_reason, reasoning, tool_call, usage};
use crate::schema::{openai_blocks, roles};
use crate::state::StreamState;

fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn ensure_state(state: &mut StreamState, model: Option<&str>) {
    if state.message_id.is_none() {
        state.message_id = Some(format!("chatcmpl-{}", now_secs() * 1000));
        state.ext.insert("created".to_string(), json!(now_secs()));
        state.model = Some(state.model.clone().or_else(|| model.map(|m| m.to_string())).unwrap_or_else(|| "commandcode".to_string()));
        state.ext.insert("chunkIndex".to_string(), json!(0));
        state.ext.insert("toolIndex".to_string(), json!(0));
        state.ext.insert("toolIndexById".to_string(), json!({}));
        state.ext.insert("openText".to_string(), json!(false));
        state.finish_reason = None;
        state.usage = None;
    }
}

fn make_chunk(state: &StreamState, delta: Json, finish_reason: Option<&str>) -> Json {
    let opts = chunk::ChunkOptions {
        id: state.message_id.clone().unwrap_or_default(),
        created: state.ext.get("created").and_then(|v| v.as_i64()).unwrap_or(0),
        model: state.model.clone().unwrap_or_default(),
    };
    chunk::build_chunk(&opts, delta, finish_reason)
}

fn tool_index(state: &mut StreamState, id: &str) -> i64 {
    let map = state.ext.get("toolIndexById").and_then(|m| m.as_object()).cloned().unwrap_or_default();
    if let Some(idx) = map.get(id).and_then(|v| v.as_i64()) {
        return idx;
    }
    let idx = state.ext.get("toolIndex").and_then(|v| v.as_i64()).unwrap_or(0);
    state.ext.insert("toolIndex".to_string(), json!(idx + 1));
    if let Some(obj) = state.ext.get_mut("toolIndexById").and_then(|m| m.as_object_mut()) {
        obj.insert(id.to_string(), json!(idx));
    }
    idx
}

/// Convert a CommandCode stream event → OpenAI chat.completion.chunk list.
pub fn commandcode_to_openai_response(chunk: Option<&Json>, state: &mut StreamState) -> Vec<Json> {
    let Some(chunk) = chunk else { return Vec::new() };

    // Already-OpenAI chunk: pass through.
    if chunk.get("object").and_then(|v| v.as_str()) == Some("chat.completion.chunk") {
        return vec![chunk.clone()];
    }

    // Tolerate raw JSON-line framing if upstream wrapper inserts it.
    let event = if let Some(line) = chunk.as_str() {
        let line = line.trim();
        if line.is_empty() || line == "[DONE]" {
            return Vec::new();
        }
        let json_line = line.strip_prefix("data:").map(|s| s.trim()).unwrap_or(line);
        match serde_json::from_str::<Json>(json_line) {
            Ok(event) => event,
            Err(_) => return Vec::new(),
        }
    } else {
        chunk.clone()
    };

    let Some(event_type) = event.get("type").and_then(|v| v.as_str()).map(|s| s.to_string()) else {
        return Vec::new();
    };

    let model = event.get("model").and_then(|v| v.as_str());
    ensure_state(state, model);
    let chunk_index = state.ext.get("chunkIndex").and_then(|v| v.as_i64()).unwrap_or(0);
    let mut out = Vec::new();

    match event_type.as_str() {
        "text-delta" => {
            let text = event.get("text").and_then(|v| v.as_str()).or_else(|| event.get("delta").and_then(|v| v.as_str())).unwrap_or("");
            if !text.is_empty() {
                let delta = if chunk_index == 0 { json!({"role": roles::ASSISTANT, "content": text}) } else { json!({"content": text}) };
                state.ext.insert("chunkIndex".to_string(), json!(chunk_index + 1));
                state.ext.insert("openText".to_string(), json!(true));
                out.push(make_chunk(state, delta, None));
            }
        }
        "reasoning-delta" => {
            let text = event.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if !text.is_empty() {
                let delta = reasoning::reasoning_delta(text, chunk_index == 0);
                state.ext.insert("chunkIndex".to_string(), json!(chunk_index + 1));
                out.push(make_chunk(state, delta, None));
            }
        }
        "tool-input-start" => {
            let id = event.get("id").and_then(|v| v.as_str()).or_else(|| event.get("toolCallId").and_then(|v| v.as_str())).unwrap_or("");
            let id_owned = if id.is_empty() { tool_call::generate_tool_call_id(0, chunk_index as usize, event.get("toolName").and_then(|v| v.as_str()).unwrap_or("")) } else { id.to_string() };
            let idx = tool_index(state, &id_owned);
            let tool_name = event.get("toolName").and_then(|v| v.as_str()).unwrap_or("");
            let mut delta = serde_json::Map::new();
            if chunk_index == 0 {
                delta.insert("role".to_string(), json!(roles::ASSISTANT));
            }
            delta.insert("tool_calls".to_string(), json!([{
                "index": idx,
                "id": id_owned,
                "type": openai_blocks::FUNCTION,
                "function": {"name": tool_name, "arguments": ""}
            }]));
            state.ext.insert("chunkIndex".to_string(), json!(chunk_index + 1));
            out.push(make_chunk(state, Json::Object(delta), None));
        }
        "tool-input-delta" => {
            let id = event.get("id").and_then(|v| v.as_str()).or_else(|| event.get("toolCallId").and_then(|v| v.as_str())).unwrap_or("");
            let idx = state.ext.get("toolIndexById").and_then(|m| m.get(id)).and_then(|v| v.as_i64());
            let Some(idx) = idx else { return Vec::new() };
            let delta_text = event.get("delta").and_then(|v| v.as_str()).or_else(|| event.get("inputTextDelta").and_then(|v| v.as_str())).unwrap_or("");
            let delta = json!({"tool_calls": [{"index": idx, "function": {"arguments": delta_text}}]});
            out.push(make_chunk(state, delta, None));
        }
        "tool-call" => {
            // Final consolidated tool call — skip if tool-input-* deltas already emitted it.
            let id = event.get("toolCallId").and_then(|v| v.as_str()).unwrap_or("");
            let already = state.ext.get("toolIndexById").and_then(|m| m.get(id)).is_some();
            if !already {
                let idx = tool_index(state, id);
                let tool_name = event.get("toolName").and_then(|v| v.as_str()).unwrap_or("");
                let args_str = match event.get("input") {
                    Some(Json::String(s)) => s.clone(),
                    Some(other) => serde_json::to_string(other).unwrap_or_else(|_| "{}".to_string()),
                    None => "{}".to_string(),
                };
                let mut delta = serde_json::Map::new();
                if chunk_index == 0 {
                    delta.insert("role".to_string(), json!(roles::ASSISTANT));
                }
                delta.insert("tool_calls".to_string(), json!([{
                    "index": idx,
                    "id": id,
                    "type": openai_blocks::FUNCTION,
                    "function": {"name": tool_name, "arguments": args_str}
                }]));
                state.ext.insert("chunkIndex".to_string(), json!(chunk_index + 1));
                out.push(make_chunk(state, Json::Object(delta), None));
            }
        }
        "finish-step" => {
            let fr = finish_reason::to_openai_finish(event.get("finishReason").and_then(|v| v.as_str()), "commandcode");
            if !fr.is_empty() {
                state.finish_reason = Some(fr.to_string());
            }
            if let Some(u) = event.get("usage").filter(|v| v.is_object()) {
                state.usage = Some(u.clone());
            }
        }
        "finish" => {
            let finish = state.finish_reason.clone().unwrap_or_else(|| {
                finish_reason::to_openai_finish(event.get("finishReason").and_then(|v| v.as_str()), "commandcode").to_string()
            });
            let final_chunk = make_chunk(state, json!({}), Some(&finish));
            let total_usage = event.get("totalUsage").cloned().or_else(|| state.usage.clone());
            if let Some(total) = total_usage.filter(|v| v.is_object()) {
                if let Some(u) = usage::to_openai_usage(&total, "commandcode") {
                    // Attach usage to the final chunk (build_chunk creates fresh one per call).
                    let mut final_chunk = final_chunk;
                    final_chunk["usage"] = u;
                    out.push(final_chunk);
                    return out;
                }
            }
            out.push(final_chunk);
        }
        // Silently ignore: start, start-step, reasoning-start/end, text-start/end,
        // provider-metadata, message-metadata, error, etc.
        _ => {}
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_text_delta_emits_role_first() {
        let mut state = StreamState::default();
        let out = commandcode_to_openai_response(Some(&json!({"type": "text-delta", "text": "hello"})), &mut state);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(out[0]["choices"][0]["delta"]["content"], "hello");
        let out2 = commandcode_to_openai_response(Some(&json!({"type": "text-delta", "text": " world"})), &mut state);
        assert_eq!(out2[0]["choices"][0]["delta"]["role"], Json::Null);
        assert_eq!(out2[0]["choices"][0]["delta"]["content"], " world");
    }

    #[test]
    fn test_tool_input_start_and_delta() {
        let mut state = StreamState::default();
        let out = commandcode_to_openai_response(Some(&json!({"type": "tool-input-start", "id": "t1", "toolName": "search"})), &mut state);
        assert_eq!(out[0]["choices"][0]["delta"]["tool_calls"][0]["id"], "t1");
        assert_eq!(out[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"], "search");
        let out2 = commandcode_to_openai_response(Some(&json!({"type": "tool-input-delta", "id": "t1", "delta": "{\"q\":\"x\"}"})), &mut state);
        assert_eq!(out2[0]["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
        assert_eq!(out2[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"], "{\"q\":\"x\"}");
    }

    #[test]
    fn test_finish_carries_usage_and_reason() {
        let mut state = StreamState::default();
        let _ = commandcode_to_openai_response(Some(&json!({"type": "text-delta", "text": "hi"})), &mut state);
        let _ = commandcode_to_openai_response(Some(&json!({"type": "finish-step", "finishReason": "tool-calls", "usage": {"inputTokens": 5, "outputTokens": 7}})), &mut state);
        let out = commandcode_to_openai_response(Some(&json!({"type": "finish", "totalUsage": {"inputTokens": 5, "outputTokens": 7}})), &mut state);
        assert_eq!(out[0]["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(out[0]["usage"]["prompt_tokens"], 5);
        assert_eq!(out[0]["usage"]["completion_tokens"], 7);
    }

    #[test]
    fn test_tool_call_after_deltas_not_duped() {
        let mut state = StreamState::default();
        let _ = commandcode_to_openai_response(Some(&json!({"type": "tool-input-start", "id": "t1", "toolName": "f"})), &mut state);
        let out = commandcode_to_openai_response(Some(&json!({"type": "tool-call", "toolCallId": "t1", "toolName": "f", "input": {}})), &mut state);
        assert_eq!(out.len(), 0);
    }

    #[test]
    fn test_string_framing_tolerated() {
        let mut state = StreamState::default();
        let out = commandcode_to_openai_response(Some(&json!("data: {\"type\": \"text-delta\", \"text\": \"x\"}")), &mut state);
        assert_eq!(out[0]["choices"][0]["delta"]["content"], "x");
    }
}
