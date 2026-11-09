//! OpenAI to Claude response translator. Port of open-sse/translator/response/openai-to-claude.js.

use nr_core::Json;
use serde_json::json;
use std::collections::HashMap;

use crate::concerns::{finish_reason, reasoning};
use crate::schema::{self, claude_blocks, claude_events, claude_deltas};
use crate::state::StreamState;

const CLAUDE_OAUTH_TOOL_PREFIX: &str = "proxy_";
const MODEL_FALLBACK: &str = "claude-3-5-sonnet-20241022";

fn sanitize_tool_args(tool_name: &str, args_json: &str) -> String {
    if let Ok(mut args) = serde_json::from_str::<serde_json::Value>(args_json) {
        let base_name = if tool_name.starts_with(CLAUDE_OAUTH_TOOL_PREFIX) {
            &tool_name[CLAUDE_OAUTH_TOOL_PREFIX.len()..]
        } else {
            tool_name
        };

        if base_name == "Read" {
            sanitize_read_args(&mut args);
        }
        serde_json::to_string(&args).unwrap_or_else(|_| args_json.to_string())
    } else {
        args_json.to_string()
    }
}

fn sanitize_read_args(args: &mut serde_json::Value) {
    if let Some(obj) = args.as_object_mut() {
        if let Some(limit) = obj.get_mut("limit") {
            if let Some(s) = limit.as_str() {
                if let Ok(n) = s.parse::<i64>() {
                    *limit = json!(n);
                }
            }
            if let Some(n) = limit.as_i64() {
                if n > 2000 {
                    *limit = json!(2000);
                } else if n < 1 {
                    obj.remove("limit");
                }
            }
        }

        if let Some(offset) = obj.get_mut("offset") {
            if let Some(s) = offset.as_str() {
                if let Ok(n) = s.parse::<i64>() {
                    *offset = json!(n);
                }
            }
            if let Some(n) = offset.as_i64() {
                if n < 0 {
                    *offset = json!(0);
                }
            }
        }

        if let Some(Json::String(file_path)) = obj.get("file_path") {
            if let Some(pages) = obj.get("pages") {
                if let Some(pages_str) = pages.as_str() {
                    let valid_pdf = file_path.to_lowercase().ends_with(".pdf");
                    let valid_pages = regex::Regex::new(r"^\d+(?:-\d+)?$")
                        .ok()
                        .map(|re| re.is_match(pages_str))
                        .unwrap_or(false);
                    if !valid_pdf || !valid_pages {
                        obj.remove("pages");
                    }
                }
            }
        }
    }
}

pub fn openai_to_claude_response(chunk: Option<&Json>, state: &mut StreamState) -> Vec<Json> {
    let chunk = match chunk {
        Some(c) => c,
        None => return Vec::new(),
    };

    if chunk.get("choices").and_then(|c| c.get(0)).is_none() {
        return Vec::new();
    }

    let mut results = Vec::new();
    let choice = &chunk["choices"][0];
    let delta = choice.get("delta");

    // Track usage
    if let Some(usage) = chunk.get("usage").and_then(|u| u.as_object()) {
        let prompt_tokens = usage.get("prompt_tokens").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let output_tokens = usage.get("completion_tokens").and_then(|v| v.as_i64()).unwrap_or(0) as i32;

        let cached_tokens = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0) as i32;

        let cache_creation = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cache_creation_tokens"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0) as i32;

        let input_tokens = prompt_tokens - cached_tokens - cache_creation;

        let mut usage_obj = json!({
            "input_tokens": input_tokens,
            "output_tokens": output_tokens
        });

        if cached_tokens > 0 {
            usage_obj["cache_read_input_tokens"] = json!(cached_tokens);
        }
        if cache_creation > 0 {
            usage_obj["cache_creation_input_tokens"] = json!(cache_creation);
        }

        state.usage = Some(usage_obj);
    }

    // First chunk: send message_start
    if !state.message_id.is_some() {
        state.message_id = Some(
            chunk
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or(&format!("msg_{}", std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()))
                .replace("chatcmpl-", "")
                .to_string(),
        );

        state.model = chunk
            .get("model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| Some(MODEL_FALLBACK.to_string()));

        results.push(json!({
            "type": "message_start",
            "message": {
                "id": state.message_id.as_deref().unwrap_or(""),
                "type": "message",
                "role": "assistant",
                "model": state.model.as_deref().unwrap_or(MODEL_FALLBACK),
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": { "input_tokens": 0, "output_tokens": 0 }
            }
        }));
    }

    // Process delta
    if let Some(delta) = delta {
        let reasoning_text = reasoning::extract_reasoning_text(delta);
        if !reasoning_text.is_empty() {
            // Stop text block if started
            if state.text_block_started {
                state.text_block_started = false;
                results.push(json!({
                    "type": "content_block_stop",
                    "index": state.current_block_index
                }));
            }

            if !state.thinking_block_started {
                let idx = state.content_block_index;
                state.content_block_index += 1;
                state.thinking_block_started = true;
                state.current_block_index = Some(idx);

                results.push(json!({
                    "type": "content_block_start",
                    "index": idx,
                    "content_block": {
                        "type": claude_blocks::THINKING,
                        "thinking": ""
                    }
                }));
            }

            if let Some(idx) = state.current_block_index {
                results.push(json!({
                    "type": "content_block_delta",
                    "index": idx,
                    "delta": {
                        "type": "thinking_delta",
                        "thinking": reasoning_text
                    }
                }));
            }
        }

        if let Some(content) = delta.get("content").and_then(|v| v.as_str()) {
            if !content.is_empty() {
                if state.thinking_block_started {
                    state.thinking_block_started = false;
                    if let Some(idx) = state.current_block_index {
                        results.push(json!({
                            "type": "content_block_stop",
                            "index": idx
                        }));
                    }
                }

                if !state.text_block_started {
                    let idx = state.content_block_index;
                    state.content_block_index += 1;
                    state.text_block_started = true;
                    state.current_block_index = Some(idx);

                    results.push(json!({
                        "type": "content_block_start",
                        "index": idx,
                        "content_block": {
                            "type": claude_blocks::TEXT,
                            "text": ""
                        }
                    }));
                }

                if let Some(idx) = state.current_block_index {
                    results.push(json!({
                        "type": "content_block_delta",
                        "index": idx,
                        "delta": {
                            "type": "text_delta",
                            "text": content
                        }
                    }));
                }
            }
        }

        // Tool calls
        if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
            for tc in tool_calls {
                if let Some(idx_val) = tc.get("index").and_then(|v| v.as_i64()) {
                    let idx = idx_val as i64;

                    if tc.get("id").is_some() && !state.tool_calls.contains_key(&idx) {
                        if state.thinking_block_started {
                            state.thinking_block_started = false;
                            if let Some(block_idx) = state.current_block_index {
                                results.push(json!({
                                    "type": "content_block_stop",
                                    "index": block_idx
                                }));
                            }
                        }
                        if state.text_block_started {
                            state.text_block_started = false;
                            if let Some(block_idx) = state.current_block_index {
                                results.push(json!({
                                    "type": "content_block_stop",
                                    "index": block_idx
                                }));
                            }
                        }

                        let tool_block_idx = state.content_block_index;
                        state.content_block_index += 1;

                        let tool_name = tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");

                        let restored_name = if tool_name.starts_with(CLAUDE_OAUTH_TOOL_PREFIX) {
                            &tool_name[CLAUDE_OAUTH_TOOL_PREFIX.len()..]
                        } else {
                            tool_name
                        };

                        state.tool_calls.insert(
                            idx,
                            json!({
                                "id": tc.get("id"),
                                "name": restored_name,
                                "blockIndex": tool_block_idx,
                                "arguments": ""
                            }),
                        );

                        results.push(json!({
                            "type": "content_block_start",
                            "index": tool_block_idx,
                            "content_block": {
                                "type": claude_blocks::TOOL_USE,
                                "id": tc.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                                "name": restored_name,
                                "input": {}
                            }
                        }));
                    }

                    if let Some(args) = tc
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(|v| v.as_str())
                    {
                        if !state.ext.contains_key("toolArgBuffers") {
                            state.ext.insert("toolArgBuffers".to_string(), json!({}));
                        }
                        if let Some(buffers) = state.ext.get_mut("toolArgBuffers") {
                            if let Some(buf_obj) = buffers.as_object_mut() {
                                let key = idx.to_string();
                                let current = buf_obj
                                    .get(&key)
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                buf_obj.insert(key, json!(format!("{}{}", current, args)));
                            }
                        }
                    }
                }
            }
        }
    }

    // Finish reason
    if let Some(finish_reason) = choice.get("finish_reason").and_then(|v| v.as_str()) {
        if !finish_reason.is_empty() {
            if state.thinking_block_started {
                state.thinking_block_started = false;
                if let Some(idx) = state.current_block_index {
                    results.push(json!({
                        "type": "content_block_stop",
                        "index": idx
                    }));
                }
            }
            if state.text_block_started {
                state.text_block_started = false;
                if let Some(idx) = state.current_block_index {
                    results.push(json!({
                        "type": "content_block_stop",
                        "index": idx
                    }));
                }
            }

            // Emit buffered tool args before finish
            if let Some(buffers) = state.ext.get("toolArgBuffers").and_then(|v| v.as_object()) {
                for (key, buffered_val) in buffers {
                    if let Ok(tool_idx) = key.parse::<i64>() {
                        if let Some(tool_info) = state.tool_calls.get(&tool_idx) {
                            if let Some(buffered) = buffered_val.as_str() {
                                let tool_name = tool_info
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                let sanitized = sanitize_tool_args(tool_name, buffered);
                                if let Some(block_idx) = tool_info.get("blockIndex").and_then(|v| v.as_i64()) {
                                    results.push(json!({
                                        "type": "content_block_delta",
                                        "index": block_idx,
                                        "delta": {
                                            "type": "input_json_delta",
                                            "partial_json": sanitized
                                        }
                                    }));
                                }
                            }
                        }
                    }
                }
            }

            // Close tool blocks
            for (_, tool_info) in &state.tool_calls {
                if let Some(block_idx) = tool_info.get("blockIndex").and_then(|v| v.as_i64()) {
                    results.push(json!({
                        "type": "content_block_stop",
                        "index": block_idx
                    }));
                }
            }

            let final_reason = finish_reason::openai_to_claude_finish_reason(finish_reason);

            let mut final_usage = json!({ "input_tokens": 0, "output_tokens": 0 });
            if let Some(usage) = &state.usage {
                final_usage = json!({
                    "input_tokens": usage.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
                    "output_tokens": usage.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
                });
            }

            results.push(json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": final_reason
                },
                "usage": final_usage
            }));

            results.push(json!({ "type": "message_stop" }));
            state.finish_reason_sent = true;
        }
    }

    if !results.is_empty() {
        results
    } else {
        Vec::new()
    }
}
