//! Ollama to OpenAI response translator. Port of open-sse/translator/response/ollama-to-openai.js.

use nr_core::Json;
use serde_json::json;

use crate::concerns::{chunk, finish_reason, tool_call, usage};
use crate::schema::{openai_blocks, openai_finish, roles};
use crate::state::StreamState;

fn convert_tool_calls(tool_calls: &[Json]) -> Vec<Json> {
    tool_calls
        .iter()
        .enumerate()
        .map(|(i, tc)| {
            let function = tc.get("function").cloned().unwrap_or(Json::Null);
            let index = function.get("index").and_then(|v| v.as_i64()).unwrap_or(i as i64);
            let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let id = if id.is_empty() { tool_call::generate_tool_call_id(0, i, function.get("name").and_then(|n| n.as_str()).unwrap_or("")) } else { id.to_string() };
            let name = function.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = match function.get("arguments") {
                Some(Json::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => "{}".to_string(),
            };
            json!({
                "index": index,
                "id": id,
                "type": openai_blocks::FUNCTION,
                "function": {"name": name, "arguments": args}
            })
        })
        .collect()
}

/// Convert an Ollama NDJSON chunk → OpenAI chat.completion.chunk.
pub fn ollama_to_openai_response(chunk: Option<&Json>, state: &mut StreamState) -> Vec<Json> {
    let Some(chunk) = chunk.filter(|c| c.is_object()) else { return Vec::new() };

    // Initialize state on first chunk.
    if state.ext.get("ollama").is_none() {
        let created = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let id = format!("chatcmpl-{created}000");
        let model = chunk.get("model").and_then(|v| v.as_str()).unwrap_or(state.model.as_deref().unwrap_or("ollama"));
        state.ext.insert("ollama".to_string(), json!({"id": id, "created": created as i64, "model": model}));
    }
    let ollama = state.ext.get("ollama").cloned().unwrap_or(json!({}));
    let id = ollama.get("id").and_then(|v| v.as_str()).unwrap_or("chatcmpl").to_string();
    let created = ollama.get("created").and_then(|v| v.as_i64()).unwrap_or(0);
    let model = ollama.get("model").and_then(|v| v.as_str()).unwrap_or("ollama").to_string();
    let opts = chunk::ChunkOptions { id: id.clone(), created, model: model.clone() };

    // Final chunk
    if chunk.get("done").and_then(|v| v.as_bool()) == Some(true) {
        let usage_json = usage::to_openai_usage(chunk, "ollama");
        let done_reason = chunk.get("done_reason").and_then(|v| v.as_str());
        let had_tool_calls = state.ext.get("hadToolCalls").and_then(|v| v.as_bool()).unwrap_or(false);
        let mut finish = finish_reason::to_openai_finish(done_reason, "ollama").to_string();
        if done_reason == Some(openai_finish::TOOL_CALLS) || had_tool_calls {
            finish = openai_finish::TOOL_CALLS.to_string();
        }
        let mut done = chunk::build_chunk(&opts, json!({}), Some(&finish));
        if let Some(u) = usage_json {
            done["usage"] = u;
        }
        return vec![done];
    }

    let Some(message) = chunk.get("message").filter(|m| m.is_object()) else { return Vec::new() };
    let content = message.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let thinking = message.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
    let tool_calls = message.get("tool_calls").and_then(|t| t.as_array()).cloned();

    if content.is_empty() && thinking.is_empty() && tool_calls.is_none() {
        return Vec::new();
    }

    let mut delta = serde_json::Map::new();
    if !content.is_empty() {
        delta.insert("content".to_string(), json!(content));
    }
    if !thinking.is_empty() {
        delta.insert("reasoning_content".to_string(), json!(thinking));
    }
    if let Some(tcs) = tool_calls {
        state.ext.insert("hadToolCalls".to_string(), json!(true));
        delta.insert("tool_calls".to_string(), Json::Array(convert_tool_calls(&tcs)));
    }
    vec![chunk::build_chunk(&opts, Json::Object(delta), None)]
}

/// Convert a full Ollama non-streaming body → OpenAI chat.completion.
pub fn ollama_body_to_openai(body: &Json) -> Json {
    let msg = body.get("message").cloned().unwrap_or_else(|| json!({}));
    let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let thinking = msg.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
    let tool_calls = msg.get("tool_calls").and_then(|t| t.as_array()).cloned().unwrap_or_default();

    let mut message = json!({"role": roles::ASSISTANT});
    if !content.is_empty() {
        message["content"] = json!(content);
    }
    if !thinking.is_empty() {
        message["reasoning_content"] = json!(thinking);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = Json::Array(convert_tool_calls(&tool_calls));
    }
    if message.get("content").is_none() && message.get("tool_calls").is_none() {
        message["content"] = json!("");
    }

    let finish_reason = if !tool_calls.is_empty() {
        openai_finish::TOOL_CALLS.to_string()
    } else {
        finish_reason::to_openai_finish(body.get("done_reason").and_then(|v| v.as_str()), "ollama").to_string()
    };
    let created = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let mut result = json!({
        "id": format!("chatcmpl-{created}000"),
        "object": "chat.completion",
        "created": created as i64,
        "model": body.get("model").and_then(|v| v.as_str()).unwrap_or("ollama"),
        "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}]
    });
    if let Some(u) = usage::to_openai_usage(body, "ollama") {
        result["usage"] = u;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ollama_content_chunk() {
        let mut state = StreamState::default();
        let chunk = json!({"model": "llama3", "message": {"role": "assistant", "content": "hi"}, "done": false});
        let out = ollama_to_openai_response(Some(&chunk), &mut state);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["choices"][0]["delta"]["content"], "hi");
        assert_eq!(out[0]["choices"][0]["finish_reason"], Json::Null);
        assert_eq!(out[0]["model"], "llama3");
    }

    #[test]
    fn test_ollama_done_chunk_with_usage() {
        let mut state = StreamState::default();
        let chunk = json!({"model": "llama3", "done": true, "prompt_eval_count": 10, "eval_count": 5});
        let out = ollama_to_openai_response(Some(&chunk), &mut state);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["choices"][0]["delta"].as_object().map(|d| d.len()).unwrap_or(0), 0);
        assert_eq!(out[0]["usage"]["prompt_tokens"], 10);
        assert_eq!(out[0]["usage"]["completion_tokens"], 5);
    }

    #[test]
    fn test_ollama_tool_calls_chunk() {
        let mut state = StreamState::default();
        let chunk = json!({"message": {"role": "assistant", "content": "", "tool_calls": [
            {"function": {"name": "lookup", "arguments": {"q": "x"}}}
        ]}, "done": false});
        let out = ollama_to_openai_response(Some(&chunk), &mut state);
        assert_eq!(out.len(), 1);
        let tc = &out[0]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tc["function"]["name"], "lookup");
        assert_eq!(tc["function"]["arguments"], "{\"q\":\"x\"}");
        // done_reason tool_calls override on subsequent done chunk
        let done = json!({"done": true, "done_reason": "stop"});
        let out_done = ollama_to_openai_response(Some(&done), &mut state);
        assert_eq!(out_done[0]["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn test_ollama_thinking_chunk_maps_reasoning_content() {
        let mut state = StreamState::default();
        let chunk = json!({"message": {"role": "assistant", "content": "", "thinking": "hm"}, "done": false});
        let out = ollama_to_openai_response(Some(&chunk), &mut state);
        assert_eq!(out[0]["choices"][0]["delta"]["reasoning_content"], "hm");
    }

    #[test]
    fn test_ollama_body_to_openai() {
        let body = json!({
            "model": "llama3",
            "message": {"role": "assistant", "content": "hello", "thinking": "why"},
            "done": true,
            "prompt_eval_count": 12,
            "eval_count": 8
        });
        let result = ollama_body_to_openai(&body);
        assert_eq!(result["object"], "chat.completion");
        let message = &result["choices"][0]["message"];
        assert_eq!(message["content"], "hello");
        assert_eq!(message["reasoning_content"], "why");
        assert_eq!(result["choices"][0]["finish_reason"], "stop");
        assert_eq!(result["usage"]["prompt_tokens"], 12);
        assert_eq!(result["usage"]["completion_tokens"], 8);
    }
}
