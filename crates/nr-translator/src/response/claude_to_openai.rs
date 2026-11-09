//! Claude to OpenAI response translator. Port of open-sse/translator/response/claude-to-openai.js.

use nr_core::Json;
use serde_json::json;

use crate::concerns::{chunk, finish_reason, reasoning, usage};
use crate::schema::{self, claude_blocks, openai_blocks, openai_finish};
use crate::state::StreamState;

/// Convert Claude stream chunk to OpenAI format
pub fn claude_to_openai_response(chunk: Option<&Json>, state: &mut StreamState) -> Vec<Json> {
    let chunk = match chunk {
        Some(c) => c,
        None => return Vec::new(),
    };

    let mut results = Vec::new();
    let event = chunk
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    match event {
        "message_start" => {
            if let Some(message) = chunk.get("message").and_then(|m| m.as_object()) {
                state.message_id = message
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| Some(format!("msg_{}", std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs())));
                state.model = message.get("model").and_then(|v| v.as_str()).map(|s| s.to_string());
            }

            state.content_block_index = 0;

            // Capture cache from message_start
            if let Some(start_usage) = chunk.get("message").and_then(|m| m.get("usage")) {
                if let Some(usage_obj) = start_usage.as_object() {
                    let input_tokens = usage_obj
                        .get("input_tokens")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0) as i32;
                    let cache_read = usage_obj
                        .get("cache_read_input_tokens")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0) as i32;
                    let cache_creation = usage_obj
                        .get("cache_creation_input_tokens")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0) as i32;
                    let prompt_tokens = input_tokens + cache_read + cache_creation;

                    let mut usage_obj = json!({
                        "prompt_tokens": prompt_tokens,
                        "completion_tokens": 0,
                        "total_tokens": prompt_tokens,
                        "input_tokens": input_tokens,
                        "output_tokens": 0
                    });
                    if cache_read > 0 {
                        usage_obj["cache_read_input_tokens"] = json!(cache_read);
                    }
                    if cache_creation > 0 {
                        usage_obj["cache_creation_input_tokens"] = json!(cache_creation);
                    }
                    state.usage = Some(usage_obj);
                }
            }

            results.push(chunk::build_chunk_for(
                &format!("chatcmpl-{}", state.message_id.as_deref().unwrap_or("unknown")),
                state.model.as_deref().unwrap_or(""),
                json!({ "role": schema::roles::ASSISTANT }),
                None,
            ));
        }

        "content_block_start" => {
            if let Some(block) = chunk.get("content_block") {
                let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");

                if block_type == "server_tool_use" {
                    state.ext.insert("serverToolBlockIndex".to_string(), chunk.get("index").cloned().unwrap_or(Json::Null));
                    return results;
                }

                if block_type == claude_blocks::TEXT {
                    state.text_block_started = true;
                } else if block_type == claude_blocks::TOOL_USE {
                    let idx = state.content_block_index;
                    state.content_block_index += 1;

                    let tool_name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    let restored_name = state
                        .tool_name_map
                        .as_ref()
                        .and_then(|m| m.get(tool_name))
                        .map(|s| s.as_str())
                        .unwrap_or(tool_name);

                    let tool_call = json!({
                        "index": idx,
                        "id": block.get("id").and_then(|v| v.as_str()).unwrap_or(&format!("call_{}", idx)),
                        "type": openai_blocks::FUNCTION,
                        "function": {
                            "name": restored_name,
                            "arguments": ""
                        }
                    });

                    state.tool_calls.insert(idx, tool_call.clone());

                    results.push(chunk::build_chunk_for(
                        &format!("chatcmpl-{}", state.message_id.as_deref().unwrap_or("unknown")),
                        state.model.as_deref().unwrap_or(""),
                        json!({ "tool_calls": [tool_call] }),
                        None,
                    ));
                }
            }
        }

        "content_block_delta" => {
            let block_index = chunk.get("index").and_then(|v| v.as_i64()).unwrap_or(0);

            // Skip deltas for server tool blocks
            if let Some(server_idx) = state.ext.get("serverToolBlockIndex") {
                if chunk.get("index") == Some(server_idx) {
                    return results;
                }
            }

            if let Some(delta) = chunk.get("delta") {
                if let Some(delta_type) = delta.get("type").and_then(|v| v.as_str()) {
                    match delta_type {
                        "text_delta" => {
                            if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                                if !text.is_empty() {
                                    results.push(chunk::build_chunk_for(
                                        &format!("chatcmpl-{}", state.message_id.as_deref().unwrap_or("unknown")),
                                        state.model.as_deref().unwrap_or(""),
                                        json!({ "content": text }),
                                        None,
                                    ));
                                }
                            }
                        }
                        "thinking_delta" => {
                            if let Some(text) = delta.get("thinking").and_then(|v| v.as_str()) {
                                if !text.is_empty() {
                                    results.push(chunk::build_chunk_for(
                                        &format!("chatcmpl-{}", state.message_id.as_deref().unwrap_or("unknown")),
                                        state.model.as_deref().unwrap_or(""),
                                        reasoning::reasoning_delta(text, false),
                                        None,
                                    ));
                                }
                            }
                        }
                        "input_json_delta" => {
                            if let Some(partial_json) = delta.get("partial_json").and_then(|v| v.as_str()) {
                                if let Some(tool_call) = state.tool_calls.get_mut(&block_index) {
                                    if let Some(func) = tool_call.get_mut("function") {
                                        if let Some(args) = func.get_mut("arguments") {
                                            if let Some(old_args) = args.as_str() {
                                                let new_args = format!("{}{}", old_args, partial_json);
                                                *args = Json::String(new_args);
                                            }
                                        }
                                    }

                                    results.push(chunk::build_chunk_for(
                                        &format!("chatcmpl-{}", state.message_id.as_deref().unwrap_or("unknown")),
                                        state.model.as_deref().unwrap_or(""),
                                        json!({
                                            "tool_calls": [{
                                                "index": block_index,
                                                "id": tool_call.get("id"),
                                                "function": {
                                                    "arguments": partial_json
                                                }
                                            }]
                                        }),
                                        None,
                                    ));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        "content_block_stop" => {
            if let Some(server_idx) = state.ext.get("serverToolBlockIndex") {
                if chunk.get("index") == Some(server_idx) {
                    state.ext.remove("serverToolBlockIndex");
                    return results;
                }
            }
            state.text_block_started = false;
            state.thinking_block_started = false;
        }

        "message_delta" => {
            if let Some(msg_usage) = chunk.get("usage") {
                if let Some(usage_obj) = msg_usage.as_object() {
                    let prev_usage = state.usage.clone().unwrap_or_else(|| json!({}));

                    let input_tokens = usage_obj
                        .get("input_tokens")
                        .and_then(|v| v.as_i64())
                        .or_else(|| prev_usage.get("input_tokens").and_then(|v| v.as_i64()))
                        .unwrap_or(0) as i32;

                    let output_tokens = usage_obj
                        .get("output_tokens")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0) as i32;

                    let cache_read = usage_obj
                        .get("cache_read_input_tokens")
                        .and_then(|v| v.as_i64())
                        .or_else(|| prev_usage.get("cache_read_input_tokens").and_then(|v| v.as_i64()))
                        .unwrap_or(0) as i32;

                    let cache_creation = usage_obj
                        .get("cache_creation_input_tokens")
                        .and_then(|v| v.as_i64())
                        .or_else(|| prev_usage.get("cache_creation_input_tokens").and_then(|v| v.as_i64()))
                        .unwrap_or(0) as i32;

                    let prompt_tokens = input_tokens + cache_read + cache_creation;

                    let mut new_usage = json!({
                        "prompt_tokens": prompt_tokens,
                        "completion_tokens": output_tokens,
                        "total_tokens": prompt_tokens + output_tokens,
                        "input_tokens": input_tokens,
                        "output_tokens": output_tokens
                    });

                    if cache_read > 0 {
                        new_usage["cache_read_input_tokens"] = json!(cache_read);
                    }
                    if cache_creation > 0 {
                        new_usage["cache_creation_input_tokens"] = json!(cache_creation);
                    }

                    state.usage = Some(new_usage);
                }
            }

            if let Some(delta) = chunk.get("delta") {
                if let Some(stop_reason) = delta.get("stop_reason").and_then(|v| v.as_str()) {
                    state.finish_reason = Some(finish_reason::openai_to_claude_finish_reason(stop_reason).to_string());

                    // Handle refusal note
                    if stop_reason == "refusal" {
                        if let Some(explanation) = delta
                            .get("stop_details")
                            .and_then(|sd| sd.get("explanation"))
                            .and_then(|v| v.as_str())
                        {
                            results.push(chunk::build_chunk_for(
                                &format!("chatcmpl-{}", state.message_id.as_deref().unwrap_or("unknown")),
                                state.model.as_deref().unwrap_or(""),
                                json!({ "content": explanation }),
                                None,
                            ));
                        }
                    }

                    let mut final_chunk = chunk::build_chunk_for(
                        &format!("chatcmpl-{}", state.message_id.as_deref().unwrap_or("unknown")),
                        state.model.as_deref().unwrap_or(""),
                        json!({}),
                        state.finish_reason.as_deref(),
                    );

                    if let Some(usage) = &state.usage {
                        let raw = json!({
                            "input_tokens": usage.get("input_tokens"),
                            "output_tokens": usage.get("output_tokens"),
                            "cache_read_input_tokens": usage.get("cache_read_input_tokens"),
                            "cache_creation_input_tokens": usage.get("cache_creation_input_tokens"),
                        });
                        if let Some(u) = usage::to_openai_usage(&raw, "claude") {
                            final_chunk["usage"] = u;
                        }
                    }

                    results.push(final_chunk);
                    state.finish_reason_sent = true;
                }
            }
        }

        "message_stop" => {
            if !state.finish_reason_sent {
                let finish_reason = state
                    .finish_reason
                    .clone()
                    .unwrap_or_else(|| {
                        if state.tool_calls.len() > 0 {
                            openai_finish::TOOL_CALLS.to_string()
                        } else {
                            openai_finish::STOP.to_string()
                        }
                    });

                let mut final_chunk = chunk::build_chunk_for(
                    &format!("chatcmpl-{}", state.message_id.as_deref().unwrap_or("unknown")),
                    state.model.as_deref().unwrap_or(""),
                    json!({}),
                    Some(&finish_reason),
                );

                if let Some(usage) = &state.usage {
                    let input_tokens = usage.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                    let output_tokens = usage.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                    final_chunk["usage"] = json!({
                        "prompt_tokens": input_tokens,
                        "completion_tokens": output_tokens,
                        "total_tokens": input_tokens + output_tokens
                    });
                }

                results.push(final_chunk);
                state.finish_reason_sent = true;
            }
        }

        _ => {}
    }

    if !results.is_empty() {
        results
    } else {
        Vec::new()
    }
}
