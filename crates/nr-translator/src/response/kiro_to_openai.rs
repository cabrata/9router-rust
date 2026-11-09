//! Kiro → OpenAI response translator. Port of open-sse/translator/response/kiro-to-openai.js.
//!
//! The Kiro executor already emits OpenAI-shaped `chat.completion.chunk` objects
//! (or raw AWS-EventStream SSE lines). This translator normalizes the raw shape
//! and restores client tool names left on `state.tool_name_map`.

use nr_core::Json;
use serde_json::json;

use crate::concerns::{chunk, finish_reason, reasoning, tool_call, usage};
use crate::schema::{openai_blocks, roles};
use crate::state::StreamState;

fn restore_tool_name(state: &StreamState, name: &str) -> String {
    state
        .tool_name_map
        .as_ref()
        .and_then(|m| m.get(name))
        .cloned()
        .unwrap_or_else(|| name.to_string())
}

fn chunk_meta(state: &StreamState) -> chunk::ChunkOptions {
    chunk::ChunkOptions {
        id: state.message_id.clone().unwrap_or_default(),
        created: state.ext.get("created").and_then(|v| v.as_i64()).unwrap_or(0),
        model: state.model.clone().unwrap_or_else(|| "kiro".to_string()),
    }
}

fn ensure_state(state: &mut StreamState) {
    if state.message_id.is_none() {
        state.message_id = Some(format!("chatcmpl-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)));
        state.ext.insert("created".to_string(), json!(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)));
        state.ext.insert("chunkIndex".to_string(), json!(0));
    }
}

fn parse_chunk(chunk: &Json) -> (Json, String) {
    if let Some(line) = chunk.as_str() {
        let mut event_type = String::new();
        let mut event_data = String::new();
        for line in line.split('\n') {
            if let Some(rest) = line.strip_prefix("event:") {
                event_type = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix(":event-type:") {
                event_type = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("data:") {
                event_data = rest.trim().to_string();
            } else if line.trim().is_empty() || line.starts_with(':') {
                // skip
            } else {
                event_data = line.trim().to_string();
            }
        }
        if event_data.is_empty() {
            return (Json::Null, event_type);
        }
        match serde_json::from_str::<Json>(&event_data) {
            Ok(mut data) => {
                if let Some(obj) = data.as_object_mut() {
                    obj.insert("_eventType".to_string(), json!(event_type));
                }
                (data, event_type)
            }
            Err(_) => (json!({"text": event_data, "_eventType": event_type}), event_type),
        }
    } else {
        let event_type = chunk.get("_eventType").and_then(|v| v.as_str()).unwrap_or("").to_string();
        (chunk.clone(), event_type)
    }
}

/// Convert a Kiro event → OpenAI chunk(s).
pub fn kiro_to_openai_response(chunk: Option<&Json>, state: &mut StreamState) -> Vec<Json> {
    let Some(chunk) = chunk else { return Vec::new() };

    // If chunk is already in OpenAI format (from executor transform), restore tool names.
    if chunk.get("object").and_then(|v| v.as_str()) == Some("chat.completion.chunk") && chunk.get("choices").is_some() {
        let Some(map) = state.tool_name_map.as_ref().filter(|m| !m.is_empty()) else {
            return vec![chunk.clone()];
        };
        let mut result = chunk.clone();
        if let Some(choices) = result.get_mut("choices").and_then(|c| c.as_array_mut()) {
            for choice in choices.iter_mut() {
                let Some(tool_calls) = choice.get_mut("delta").and_then(|d| d.get_mut("tool_calls")).and_then(|t| t.as_array_mut()) else { continue };
                for tc in tool_calls.iter_mut() {
                    if let Some(name) = tc.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str()).map(|s| s.to_string()) {
                        let restored = map.get(&name).cloned().unwrap_or(name);
                        tc["function"]["name"] = json!(restored);
                    }
                }
            }
        }
        return vec![result];
    }

    let (data, event_type) = parse_chunk(chunk);
    if data.is_null() {
        return Vec::new();
    }

    ensure_state(state);
    let event_type = if event_type.is_empty() {
        data.get("_eventType").and_then(|v| v.as_str()).unwrap_or("").to_string()
    } else {
        event_type
    };

    // assistantResponseEvent
    if event_type == "assistantResponseEvent" || data.get("assistantResponseEvent").is_some() {
        let content = data.get("assistantResponseEvent").and_then(|e| e.get("content")).and_then(|v| v.as_str()).or_else(|| data.get("content").and_then(|v| v.as_str())).unwrap_or("");
        if content.is_empty() {
            return Vec::new();
        }
        let chunk_index = state.ext.get("chunkIndex").and_then(|v| v.as_i64()).unwrap_or(0);
        let mut delta = serde_json::Map::new();
        if chunk_index == 0 {
            delta.insert("role".to_string(), json!(roles::ASSISTANT));
        }
        delta.insert("content".to_string(), json!(content));
        state.ext.insert("chunkIndex".to_string(), json!(chunk_index + 1));
        return vec![chunk::build_chunk(&chunk_meta(state), Json::Object(delta), None)];
    }

    // reasoningContentEvent
    if event_type == "reasoningContentEvent" || data.get("reasoningContentEvent").is_some() {
        let reasoning_obj = data.get("reasoningContentEvent").cloned().unwrap_or_else(|| data.clone());
        let content = if reasoning_obj.is_string() {
            reasoning_obj.as_str().unwrap_or("").to_string()
        } else {
            reasoning_obj.get("text").and_then(|v| v.as_str()).or_else(|| reasoning_obj.get("content").and_then(|v| v.as_str())).or_else(|| data.get("content").and_then(|v| v.as_str())).unwrap_or("").to_string()
        };
        if content.is_empty() {
            return Vec::new();
        }
        let chunk_index = state.ext.get("chunkIndex").and_then(|v| v.as_i64()).unwrap_or(0);
        state.ext.insert("chunkIndex".to_string(), json!(chunk_index + 1));
        return vec![chunk::build_chunk(&chunk_meta(state), reasoning::reasoning_delta(&content, chunk_index == 0), None)];
    }

    // toolUseEvent
    if event_type == "toolUseEvent" || data.get("toolUseEvent").is_some() {
        state.ext.insert("hadToolUse".to_string(), json!(true));
        let tool_use = data.get("toolUseEvent").cloned().unwrap_or_else(|| data.clone());
        let id = tool_use.get("toolUseId").and_then(|v| v.as_str()).unwrap_or("");
        let id_owned = if id.is_empty() { tool_call::generate_tool_call_id(0, 0, tool_use.get("name").and_then(|v| v.as_str()).unwrap_or("")) } else { id.to_string() };
        let name = restore_tool_name(state, tool_use.get("name").and_then(|v| v.as_str()).unwrap_or(""));
        let input = tool_use.get("input").cloned().unwrap_or_else(|| json!({}));
        let chunk_index = state.ext.get("chunkIndex").and_then(|v| v.as_i64()).unwrap_or(0);
        let mut delta = serde_json::Map::new();
        if chunk_index == 0 {
            delta.insert("role".to_string(), json!(roles::ASSISTANT));
        }
        delta.insert("tool_calls".to_string(), json!([{
            "index": 0,
            "id": id_owned,
            "type": openai_blocks::FUNCTION,
            "function": {"name": name, "arguments": serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string())}
        }]));
        state.ext.insert("chunkIndex".to_string(), json!(chunk_index + 1));
        return vec![chunk::build_chunk(&chunk_meta(state), Json::Object(delta), None)];
    }

    // messageStopEvent / done
    if event_type == "messageStopEvent" || event_type == "done" || data.get("messageStopEvent").is_some() {
        let had_tool_use = state.ext.get("hadToolUse").and_then(|v| v.as_bool()).unwrap_or(false);
        let finish = finish_reason::to_openai_finish(Some(if had_tool_use { "tool_use" } else { "stop" }), "kiro");
        state.finish_reason = Some(finish.to_string());
        let mut result = chunk::build_chunk(&chunk_meta(state), json!({}), Some(finish));
        if let Some(u) = state.usage.clone() {
            result["usage"] = u;
        }
        return vec![result];
    }

    // usageEvent
    if event_type == "usageEvent" || data.get("usageEvent").is_some() {
        let u = data.get("usageEvent").cloned().unwrap_or_else(|| data.clone());
        if let Some(openai_usage) = usage::to_openai_usage(&u, "kiro") {
            state.usage = Some(openai_usage);
        }
        return Vec::new();
    }

    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_assistant_response_event() {
        let mut state = StreamState::default();
        let chunk = json!({"_eventType": "assistantResponseEvent", "assistantResponseEvent": {"content": "hi"}});
        let out = kiro_to_openai_response(Some(&chunk), &mut state);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(out[0]["choices"][0]["delta"]["content"], "hi");
    }

    #[test]
    fn test_reasoning_content_event() {
        let mut state = StreamState::default();
        let chunk = json!({"_eventType": "reasoningContentEvent", "reasoningContentEvent": {"text": "thinking"}});
        let out = kiro_to_openai_response(Some(&chunk), &mut state);
        assert_eq!(out[0]["choices"][0]["delta"]["reasoning_content"], "thinking");
    }

    #[test]
    fn test_tool_use_event_restores_name() {
        let mut state = StreamState::default();
        state.tool_name_map = Some([("lookup__x".to_string(), "lookup".to_string())].into_iter().collect());
        let chunk = json!({"_eventType": "toolUseEvent", "toolUseEvent": {"toolUseId": "t1", "name": "lookup__x", "input": {"q": "x"}}});
        let out = kiro_to_openai_response(Some(&chunk), &mut state);
        let tc = &out[0]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tc["id"], "t1");
        assert_eq!(tc["function"]["name"], "lookup");
        assert_eq!(tc["function"]["arguments"], "{\"q\":\"x\"}");
    }

    #[test]
    fn test_message_stop_sets_finish_reason() {
        let mut state = StreamState::default();
        let _ = kiro_to_openai_response(Some(&json!({"_eventType": "toolUseEvent", "toolUseEvent": {"toolUseId": "t1", "name": "f", "input": {}}})), &mut state);
        let out = kiro_to_openai_response(Some(&json!({"_eventType": "messageStopEvent"})), &mut state);
        assert_eq!(out[0]["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn test_openai_passthrough_restores_tool_names() {
        let mut state = StreamState::default();
        state.tool_name_map = Some([("f__x".to_string(), "f".to_string())].into_iter().collect());
        let chunk = json!({
            "object": "chat.completion.chunk",
            "choices": [{"delta": {"tool_calls": [{"id": "c1", "function": {"name": "f__x", "arguments": "{}"}}]}}]
        });
        let out = kiro_to_openai_response(Some(&chunk), &mut state);
        assert_eq!(out[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"], "f");
    }
}
