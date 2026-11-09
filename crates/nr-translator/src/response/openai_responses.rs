//! OpenAI Responses API → OpenAI Chat Completions response translator.
//! Port of open-sse/translator/response/openai-responses.js (openaiResponsesToOpenAIResponse).

use std::collections::HashSet;

use nr_core::Json;
use serde_json::json;

use crate::concerns::{chunk, reasoning, tool_call, usage};
use crate::schema::{defaults, openai_blocks, openai_finish, responses_events, responses_items};
use crate::state::StreamState;

fn now_millis() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

fn compute_finish_reason(state: &StreamState) -> &'static str {
    let had_tool = state.ext.get("respHadToolCalls").and_then(|v| v.as_bool()).unwrap_or(false);
    if had_tool {
        openai_finish::TOOL_CALLS
    } else if state.finish_reason.as_deref() == Some(openai_finish::LENGTH) {
        openai_finish::LENGTH
    } else {
        openai_finish::STOP
    }
}

fn final_chunk(state: &StreamState, finish_reason: &str) -> Json {
    let id = state
        .ext
        .get("chatId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("chatcmpl-{}", now_millis()));
    let opts = chunk::ChunkOptions {
        id,
        created: state.ext.get("created").and_then(|v| v.as_i64()).unwrap_or_else(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)),
        model: state.model.clone().unwrap_or_else(|| defaults::MODEL_FALLBACK.to_string()),
    };
    let mut result = chunk::build_chunk(&opts, json!({}), Some(finish_reason));
    if let Some(u) = state.usage.clone().filter(|v| v.is_object()) {
        result["usage"] = u;
    }
    result
}

fn ensure_state(state: &mut StreamState) {
    if state.ext.get("started").is_none() {
        state.ext.insert("started".to_string(), json!(true));
        state.ext.insert("chatId".to_string(), json!(format!("chatcmpl-{}", now_millis())));
        state.ext.insert("created".to_string(), json!(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)));
        state.ext.insert("toolCallIndex".to_string(), json!(0));
        state.ext.insert("respToolChatIndex".to_string(), json!({}));
        state.ext.insert("respToolArgsEmitted".to_string(), json!([]));
    }
}

fn set_key_path(body: &mut nr_core::JsonMap, path: &str, value: Json) {
    match path.split_once('/') {
        Some((parent, key)) => {
            if !body.get(parent).map(|v| v.is_object()).unwrap_or(false) {
                body.insert(parent.to_string(), json!({}));
            }
            if let Some(obj) = body.get_mut(parent).and_then(|v| v.as_object_mut()) {
                obj.insert(key.to_string(), value);
            }
        }
        None => {
            body.insert(path.to_string(), value);
        }
    }
}

/// Convert a Responses API event → OpenAI chat.completion.chunk(s).
pub fn openai_responses_response(chunk: Option<&Json>, state: &mut StreamState) -> Vec<Json> {
    let Some(chunk) = chunk else {
        // Flush: send final chunk with finish_reason.
        if state.ext.get("finishReasonSent").and_then(|v| v.as_bool()).unwrap_or(false) || state.ext.get("started").is_none() {
            return Vec::new();
        }
        let finish_reason = compute_finish_reason(state);
        state.ext.insert("finishReasonSent".to_string(), json!(true));
        state.finish_reason = Some(finish_reason.to_string());
        return vec![final_chunk(state, finish_reason)];
    };

    let event_type = chunk.get("type").and_then(|v| v.as_str()).or_else(|| chunk.get("event").and_then(|v| v.as_str())).unwrap_or("").to_string();
    let data = chunk.get("data").cloned().unwrap_or_else(|| chunk.clone());

    ensure_state(state);

    match event_type.as_str() {
        responses_events::OUTPUT_TEXT_DELTA => {
            let delta = data.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            if delta.is_empty() {
                return Vec::new();
            }
            let opts = chunk::ChunkOptions {
                id: state.ext.get("chatId").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                created: state.ext.get("created").and_then(|v| v.as_i64()).unwrap_or(0),
                model: state.model.clone().unwrap_or_else(|| defaults::MODEL_FALLBACK.to_string()),
            };
            return vec![chunk::build_chunk(&opts, json!({"content": delta}), None)];
        }
        responses_events::OUTPUT_TEXT_DONE => return Vec::new(),
        responses_events::OUTPUT_ITEM_ADDED => {
            let item = data.get("item").cloned().unwrap_or(Json::Null);
            let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if item_type == responses_items::FUNCTION_CALL || item_type == "custom_tool_call" {
                let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                let current_tool_call_id = if call_id.is_empty() { tool_call::generate_tool_call_id(0, 0, item.get("name").and_then(|v| v.as_str()).unwrap_or("")) } else { call_id.to_string() };
                state.ext.insert("currentToolCallId".to_string(), json!(current_tool_call_id));
                let key = item.get("id").and_then(|v| v.as_str()).or_else(|| data.get("item_id").and_then(|v| v.as_str())).unwrap_or("").to_string();
                let map = state.ext.get("respToolChatIndex").and_then(|m| m.as_object()).cloned().unwrap_or_default();
                let idx = if !key.is_empty() && map.contains_key(&key) {
                    map.get(&key).and_then(|v| v.as_i64()).unwrap_or(0)
                } else {
                    let idx = state.ext.get("toolCallIndex").and_then(|v| v.as_i64()).unwrap_or(0);
                    state.ext.insert("toolCallIndex".to_string(), json!(idx + 1));
                    if !key.is_empty() {
                        let key_path = format!("respToolChatIndex/{}", key);
                        set_key_path(&mut state.ext, &key_path, json!(idx));
                    }
                    idx
                };
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let opts = chunk::ChunkOptions {
                    id: state.ext.get("chatId").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    created: state.ext.get("created").and_then(|v| v.as_i64()).unwrap_or(0),
                    model: state.model.clone().unwrap_or_else(|| defaults::MODEL_FALLBACK.to_string()),
                };
                state.ext.insert("respHadToolCalls".to_string(), json!(true));
                return vec![chunk::build_chunk(&opts, json!({
                    "tool_calls": [{
                        "index": idx,
                        "id": current_tool_call_id,
                        "type": openai_blocks::FUNCTION,
                        "function": {"name": name, "arguments": ""}
                    }]
                }), None)];
            }
        }
        responses_events::FUNCTION_CALL_ARGUMENTS_DELTA | responses_events::CUSTOM_TOOL_CALL_INPUT_DELTA => {
            let args_delta = data.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            if args_delta.is_empty() {
                return Vec::new();
            }
            let item_id = data.get("item_id").and_then(|v| v.as_str()).unwrap_or("");
            let map = state.ext.get("respToolChatIndex").and_then(|m| m.as_object()).cloned().unwrap_or_default();
            let idx = if !item_id.is_empty() {
                map.get(item_id).and_then(|v| v.as_i64())
            } else {
                None
            }
            .unwrap_or_else(|| (state.ext.get("toolCallIndex").and_then(|v| v.as_i64()).unwrap_or(1) - 1).max(0));
            let mut emitted: HashSet<String> = state
                .ext
                .get("respToolArgsEmitted")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            emitted.insert(idx.to_string());
            let arr: Vec<Json> = emitted.iter().map(|s| json!(s)).collect();
            state.ext.insert("respToolArgsEmitted".to_string(), Json::Array(arr));

            let opts = chunk::ChunkOptions {
                id: state.ext.get("chatId").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                created: state.ext.get("created").and_then(|v| v.as_i64()).unwrap_or(0),
                model: state.model.clone().unwrap_or_else(|| defaults::MODEL_FALLBACK.to_string()),
            };
            return vec![chunk::build_chunk(&opts, json!({
                "tool_calls": [{"index": idx, "function": {"arguments": args_delta}}]
            }), None)];
        }
        responses_events::COMPLETED => {
            let response = data.get("response").cloned().unwrap_or(Json::Null);
            let usage_obj = response.get("usage").cloned().unwrap_or(Json::Null);
            if usage_obj.is_object() {
                let input_tokens = usage_obj.get("input_tokens").and_then(|v| v.as_i64()).or_else(|| usage_obj.get("prompt_tokens").and_then(|v| v.as_i64())).unwrap_or(0);
                let output_tokens = usage_obj.get("output_tokens").and_then(|v| v.as_i64()).or_else(|| usage_obj.get("completion_tokens").and_then(|v| v.as_i64())).unwrap_or(0);
                let cache_read = usage_obj.get("input_tokens_details").and_then(|d| d.get("cached_tokens")).and_then(|v| v.as_i64()).or_else(|| usage_obj.get("cache_read_input_tokens").and_then(|v| v.as_i64())).unwrap_or(0);
                state.usage = Some(usage::build_usage(&usage::UsageParams {
                    prompt_tokens: input_tokens,
                    completion_tokens: output_tokens,
                    total_tokens: input_tokens + output_tokens,
                    cached_tokens: cache_read,
                    ..Default::default()
                }));
            }
            if state.ext.get("finishReasonSent").and_then(|v| v.as_bool()).unwrap_or(false) {
                return Vec::new();
            }
            let finish_reason = compute_finish_reason(state);
            state.ext.insert("finishReasonSent".to_string(), json!(true));
            state.finish_reason = Some(finish_reason.to_string());
            return vec![final_chunk(state, finish_reason)];
        }
        responses_events::FAILED => {
            if state.ext.get("finishReasonSent").and_then(|v| v.as_bool()).unwrap_or(false) {
                return Vec::new();
            }
            let error = data.get("error").cloned().or_else(|| data.get("response").and_then(|r| r.get("error")).cloned());
            if let Some(error) = error.filter(|v| v.is_object()) {
                state.ext.insert("finishReasonSent".to_string(), json!(true));
                let message = error.get("message").and_then(|v| v.as_str()).map(|s| s.to_string()).unwrap_or_else(|| error.to_string());
                let opts = chunk::ChunkOptions {
                    id: state.ext.get("chatId").and_then(|v| v.as_str()).map(|s| s.to_string()).unwrap_or_else(|| format!("chatcmpl-{}", now_millis())),
                    created: state.ext.get("created").and_then(|v| v.as_i64()).unwrap_or(0),
                    model: state.model.clone().unwrap_or_else(|| defaults::MODEL_FALLBACK.to_string()),
                };
                return vec![chunk::build_chunk(&opts, json!({"content": format!("[Error] {message}")}), Some(openai_finish::STOP))];
            }
        }
        "response.reasoning_summary_text.delta" => {
            let delta = data.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            if delta.is_empty() {
                return Vec::new();
            }
            let opts = chunk::ChunkOptions {
                id: state.ext.get("chatId").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                created: state.ext.get("created").and_then(|v| v.as_i64()).unwrap_or(0),
                model: state.model.clone().unwrap_or_else(|| defaults::MODEL_FALLBACK.to_string()),
            };
            return vec![chunk::build_chunk(&opts, reasoning::reasoning_delta(delta, false), None)];
        }
        _ => {}
    }

    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts_of(state: &StreamState) -> (String, i64, String) {
        (
            state.ext.get("chatId").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            state.ext.get("created").and_then(|v| v.as_i64()).unwrap_or(0),
            state.model.clone().unwrap_or_default(),
        )
    }

    #[test]
    fn test_text_delta() {
        let mut state = StreamState::default();
        let chunk = json!({
            "type": responses_events::OUTPUT_TEXT_DELTA,
            "data": {"delta": "hi"}
        });
        let out = openai_responses_response(Some(&chunk), &mut state);
        assert_eq!(out[0]["choices"][0]["delta"]["content"], "hi");
        assert!(out[0]["id"].as_str().unwrap().starts_with("chatcmpl-"));
    }

    #[test]
    fn test_function_call_start_then_delta() {
        let mut state = StreamState::default();
        let added = json!({
            "type": responses_events::OUTPUT_ITEM_ADDED,
            "data": {"item": {"type": "function_call", "id": "fc1", "call_id": "call_1", "name": "search"}}
        });
        let out = openai_responses_response(Some(&added), &mut state);
        assert_eq!(out[0]["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
        assert_eq!(out[0]["choices"][0]["delta"]["tool_calls"][0]["id"], "call_1");
        assert_eq!(out[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"], "search");

        let delta = json!({
            "type": responses_events::FUNCTION_CALL_ARGUMENTS_DELTA,
            "data": {"delta": "{\"q\":\"x\"}", "item_id": "fc1"}
        });
        let out2 = openai_responses_response(Some(&delta), &mut state);
        assert_eq!(out2[0]["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
        assert_eq!(out2[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"], "{\"q\":\"x\"}");
    }

    #[test]
    fn test_parallel_calls_keyed_by_item_id() {
        let mut state = StreamState::default();
        let add_a = json!({"type": responses_events::OUTPUT_ITEM_ADDED, "data": {"item": {"type": "function_call", "id": "fcA", "call_id": "cA", "name": "a"}}});
        let add_b = json!({"type": responses_events::OUTPUT_ITEM_ADDED, "data": {"item": {"type": "function_call", "id": "fcB", "call_id": "cB", "name": "b"}}});
        let _ = openai_responses_response(Some(&add_a), &mut state);
        let _ = openai_responses_response(Some(&add_b), &mut state);
        let delta_b = json!({"type": responses_events::FUNCTION_CALL_ARGUMENTS_DELTA, "data": {"delta": "{}", "item_id": "fcB"}});
        let out = openai_responses_response(Some(&delta_b), &mut state);
        assert_eq!(out[0]["choices"][0]["delta"]["tool_calls"][0]["index"], 1);
    }

    #[test]
    fn test_completed_emits_finish_and_usage() {
        let mut state = StreamState::default();
        let _ = openai_responses_response(Some(&json!({
            "type": responses_events::OUTPUT_TEXT_DELTA, "data": {"delta": "hi"}
        })), &mut state);
        let completed = json!({
            "type": responses_events::COMPLETED,
            "data": {"response": {"usage": {"input_tokens": 3, "output_tokens": 4, "input_tokens_details": {"cached_tokens": 1}}}}
        });
        let out = openai_responses_response(Some(&completed), &mut state);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["choices"][0]["finish_reason"], "stop");
        assert_eq!(out[0]["usage"]["prompt_tokens"], 3);
        assert_eq!(out[0]["usage"]["completion_tokens"], 4);
        assert_eq!(out[0]["usage"]["prompt_tokens_details"]["cached_tokens"], 1);
    }

    #[test]
    fn test_flush_after_completion_is_noop() {
        let mut state = StreamState::default();
        let _ = openai_responses_response(Some(&json!({
            "type": responses_events::COMPLETED,
            "data": {"response": {"usage": {"input_tokens": 1, "output_tokens": 1}}}
        })), &mut state);
        let out = openai_responses_response(None, &mut state);
        assert!(out.is_empty());
    }

    #[test]
    fn test_failed_event_surfaces_error() {
        let mut state = StreamState::default();
        let failed = json!({
            "type": responses_events::FAILED,
            "data": {"error": {"message": "model_not_found"}}
        });
        let out = openai_responses_response(Some(&failed), &mut state);
        assert_eq!(out[0]["choices"][0]["delta"]["content"], "[Error] model_not_found");
        assert_eq!(out[0]["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn test_tool_calls_drive_finish_reason() {
        let mut state = StreamState::default();
        let added = json!({
            "type": responses_events::OUTPUT_ITEM_ADDED,
            "data": {"item": {"type": "function_call", "id": "fc1", "call_id": "call_1", "name": "search"}}
        });
        let _ = openai_responses_response(Some(&added), &mut state);
        let out = openai_responses_response(None, &mut state);
        assert_eq!(out[0]["choices"][0]["finish_reason"], "tool_calls");
        let _ = opts_of(&state);
    }
}
