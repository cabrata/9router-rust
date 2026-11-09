//! Gemini to OpenAI response translator. Port of open-sse/translator/response/gemini-to-openai.js.

use nr_core::Json;
use serde_json::json;

use crate::concerns::{chunk, finish_reason, reasoning, usage};
use crate::schema::{self, openai_blocks, openai_finish};
use crate::state::StreamState;

pub fn gemini_to_openai_response(chunk_opt: Option<&Json>, state: &mut StreamState) -> Vec<Json> {
    let chunk = match chunk_opt {
        Some(c) => c,
        None => return Vec::new(),
    };

    let response = chunk.get("response").unwrap_or(chunk);
    if response.get("candidates").and_then(|c| c.get(0)).is_none() {
        return Vec::new();
    }

    let mut results = Vec::new();
    let candidate = &response["candidates"][0];
    let content = candidate.get("content");

    // Initialize state
    if state.message_id.is_none() {
        let id = response
            .get("responseId")
            .and_then(|v| v.as_str())
            .unwrap_or(&format!("msg_{}", std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()))
            .to_string();

        state.message_id = Some(id);
        state.model = response
            .get("modelVersion")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| Some("gemini".to_string()));

        state.ext.insert("functionIndex".to_string(), json!(0));
        state.ext.insert("geminiToolCallCount".to_string(), json!(0));

        results.push(chunk::build_chunk(
            &format!("chatcmpl-{}", state.message_id.as_deref().unwrap_or("unknown")),
            state.model.as_deref().unwrap_or(""),
            &json!({ "role": schema::roles::ASSISTANT }),
            None,
        ));
    }

    // Process parts
    if let Some(parts) = content.and_then(|c| c.get("parts")).and_then(|p| p.as_array()) {
        for part in parts {
            let has_thought_sig = part
                .get("thoughtSignature")
                .or_else(|| part.get("thought_signature"))
                .is_some();

            let is_thought = part
                .get("thought")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if has_thought_sig {
                let has_text = part
                    .get("text")
                    .and_then(|v| v.as_str())
                    .map(|s| !s.is_empty())
                    .unwrap_or(false);

                let has_func_call = part.get("functionCall").is_some();

                if !has_text && !has_func_call {
                    continue;
                }

                if has_text {
                    if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                        let delta = if is_thought {
                            reasoning::reasoning_delta(text)
                        } else {
                            json!({ "content": text })
                        };

                        results.push(chunk::build_chunk(
                            &format!("chatcmpl-{}", state.message_id.as_deref().unwrap_or("unknown")),
                            state.model.as_deref().unwrap_or(""),
                            &delta,
                            None,
                        ));
                    }
                }

                if has_func_call {
                    if let Some(func_call) = part.get("functionCall") {
                        let thought_sig = part.get("thoughtSignature").or(part.get("thought_signature"));
                        emit_function_call(&mut results, state, func_call, thought_sig);
                        state.ext.remove("pendingThoughtSignature");
                    }
                }
                continue;
            }

            // Text content
            if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                if !text.is_empty() {
                    let delta = if is_thought {
                        reasoning::reasoning_delta(text)
                    } else {
                        json!({ "content": text })
                    };

                    results.push(chunk::build_chunk(
                        &format!("chatcmpl-{}", state.message_id.as_deref().unwrap_or("unknown")),
                        state.model.as_deref().unwrap_or(""),
                        &delta,
                        None,
                    ));
                }
            }

            // Function call
            if let Some(func_call) = part.get("functionCall") {
                let sig = state.ext.get("pendingThoughtSignature").cloned();
                emit_function_call(&mut results, state, func_call, sig.as_ref());
                state.ext.remove("pendingThoughtSignature");
            }

            // Inline data (images)
            if let Some(inline_data) = part.get("inlineData").or(part.get("inline_data")) {
                if let Some(data) = inline_data.get("data").and_then(|v| v.as_str()) {
                    let mime_type = inline_data
                        .get("mimeType")
                        .or(inline_data.get("mime_type"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("image/png");

                    let data_uri = format!("data:{};base64,{}", mime_type, data);

                    results.push(chunk::build_chunk(
                        &format!("chatcmpl-{}", state.message_id.as_deref().unwrap_or("unknown")),
                        state.model.as_deref().unwrap_or(""),
                        &json!({
                            "images": [{
                                "type": openai_blocks::IMAGE_URL,
                                "image_url": { "url": data_uri }
                            }]
                        }),
                        None,
                    ));
                }
            }
        }
    }

    // Usage metadata
    let usage_meta = response
        .get("usageMetadata")
        .or(chunk.get("usageMetadata"));
    if let Some(usage_data) = usage_meta {
        let usage_obj = usage::to_openai_usage(Some(usage_data), "gemini");
        state.usage = Some(usage_obj);
    }

    // Finish reason
    if let Some(finish_reason) = candidate.get("finishReason").and_then(|v| v.as_str()) {
        let mut resolved_reason = finish_reason::gemini_to_openai_finish_reason(finish_reason);

        let tool_count = state.ext.get("geminiToolCallCount").and_then(|v| v.as_i64()).unwrap_or(0);
        if resolved_reason == openai_finish::STOP && tool_count > 0 {
            resolved_reason = openai_finish::TOOL_CALLS;
        }

        let mut final_chunk = chunk::build_chunk(
            &format!("chatcmpl-{}", state.message_id.as_deref().unwrap_or("unknown")),
            state.model.as_deref().unwrap_or(""),
            &json!({}),
            Some(resolved_reason),
        );

        if let Some(usage) = &state.usage {
            final_chunk["usage"] = usage.clone();
        }

        results.push(final_chunk);
        state.finish_reason = Some(resolved_reason.to_string());
    }

    if !results.is_empty() {
        results
    } else {
        Vec::new()
    }
}

fn emit_function_call(
    results: &mut Vec<Json>,
    state: &mut StreamState,
    func_call: &Json,
    _sig: Option<&Json>,
) {
    let raw_name = func_call
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let restored_name = state
        .tool_name_map
        .as_ref()
        .and_then(|m| m.get(raw_name))
        .map(|s| s.as_str())
        .unwrap_or(raw_name);

    let func_args = func_call.get("args").cloned().unwrap_or(json!({}));

    let func_index = state.ext.get("functionIndex").and_then(|v| v.as_i64()).unwrap_or(0);
    let call_id = func_call
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or(&format!("{}-{}-{}", restored_name, std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(), func_index));

    let tool_call = json!({
        "id": call_id,
        "index": func_index,
        "type": openai_blocks::FUNCTION,
        "function": {
            "name": restored_name,
            "arguments": serde_json::to_string(&func_args).unwrap_or_else(|_| "{}".to_string())
        }
    });

    state.ext.insert("functionIndex".to_string(), json!(func_index + 1));
    state.ext.insert("geminiToolCallCount".to_string(), json!(state.ext
        .get("geminiToolCallCount")
        .and_then(|v| v.as_i64())
        .unwrap_or(0) + 1));

    results.push(chunk::build_chunk(
        &format!("chatcmpl-{}", state.message_id.as_deref().unwrap_or("unknown")),
        state.model.as_deref().unwrap_or(""),
        &json!({ "tool_calls": [tool_call] }),
        None,
    ));
}

// Finish reason mapping
mod finish_reason {
    use crate::schema::openai_finish;

    pub fn gemini_to_openai_finish_reason(reason: &str) -> &'static str {
        match reason {
            "STOP" => openai_finish::STOP,
            "MAX_TOKENS" => openai_finish::LENGTH,
            "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" => openai_finish::CONTENT_FILTER,
            _ => openai_finish::STOP,
        }
    }
}
