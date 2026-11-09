//! Gemini to OpenAI request translator. Port of open-sse/translator/request/gemini-to-openai.js.

use nr_core::{Credentials, Json};
use crate::{TranslationOptions, schema::*, concerns::*};

/// Convert Gemini request to OpenAI format.
pub fn gemini_to_openai_request(
    model: &str,
    body: Json,
    stream: bool,
    _credentials: Option<&mut Credentials>,
    _options: &TranslationOptions,
) -> Json {
    let mut result = serde_json::json!({
        "model": model,
        "messages": [],
        "stream": stream
    });

    // Generation config
    if let Some(gen_config) = body.get("generationConfig").and_then(|v| v.as_object()) {
        if let Some(max_output_tokens) = gen_config.get("maxOutputTokens").and_then(|v| v.as_u64()) {
            let temp_body = serde_json::json!({ "max_tokens": max_output_tokens, "tools": body.get("tools") });
            result["max_tokens"] = Json::Number((adjust_max_tokens(&temp_body) as i64).into());
        }
        if let Some(temperature) = gen_config.get("temperature").and_then(|v| v.as_f64()) {
            result["temperature"] = serde_json::json!(temperature);
        }
        if let Some(top_p) = gen_config.get("topP").and_then(|v| v.as_f64()) {
            result["top_p"] = serde_json::json!(top_p);
        }
    }

    // System instruction
    if let Some(system_instr) = body.get("systemInstruction") {
        if let Some(system_text) = extract_gemini_text(system_instr) {
            if !system_text.is_empty() {
                result["messages"].as_array_mut().unwrap().push(serde_json::json!({
                    "role": roles::SYSTEM,
                    "content": system_text
                }));
            }
        }
    }

    // Convert contents to messages
    if let Some(contents) = body.get("contents").and_then(|v| v.as_array()) {
        for content in contents {
            if let Some(converted) = convert_gemini_content(content) {
                match converted {
                    Json::Array(msgs) => {
                        for msg in msgs {
                            result["messages"].as_array_mut().unwrap().push(msg);
                        }
                    }
                    _ => {
                        result["messages"].as_array_mut().unwrap().push(converted);
                    }
                }
            }
        }
    }

    // Tools
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        let mut tool_list = Vec::new();
        for tool in tools {
            if let Some(func_decls) = tool.get("functionDeclarations").and_then(|v| v.as_array()) {
                for func in func_decls {
                    let name = func.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let desc = func.get("description").and_then(|v| v.as_str()).unwrap_or("");
                    let params = func.get("parameters").cloned().unwrap_or_else(|| {
                        serde_json::json!({"type": "object", "properties": {}})
                    });

                    tool_list.push(serde_json::json!({
                        "type": openai_blocks::FUNCTION,
                        "function": {
                            "name": name,
                            "description": desc,
                            "parameters": params
                        }
                    }));
                }
            }
        }
        if !tool_list.is_empty() {
            result["tools"] = Json::Array(tool_list);
        }
    }

    result
}

/// Convert Gemini content to OpenAI message (may return array for multiple messages).
fn convert_gemini_content(content: &Json) -> Option<Json> {
    let role_val = content.get("role")?.as_str()?;
    let role = if role_val == gemini_roles::USER {
        roles::USER
    } else {
        roles::ASSISTANT
    };

    let parts = content.get("parts")?.as_array()?;
    if parts.is_empty() {
        return None;
    }

    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();
    let mut tool_results = Vec::new();

    for part in parts {
        // Text content
        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
            text_parts.push(serde_json::json!({
                "type": openai_blocks::TEXT,
                "text": text
            }));
        }

        // Inline image data
        if let Some(inline_data) = part.get("inlineData").and_then(|v| v.as_object()) {
            if let (Some(mime_type), Some(data)) = (
                inline_data.get("mimeType").and_then(|v| v.as_str()),
                inline_data.get("data").and_then(|v| v.as_str()),
            ) {
                let data_uri = image::encode_data_uri(mime_type, data);
                text_parts.push(serde_json::json!({
                    "type": openai_blocks::IMAGE_URL,
                    "image_url": { "url": data_uri }
                }));
            }
        }

        // Function call
        if let Some(func_call) = part.get("functionCall").and_then(|v| v.as_object()) {
            let call_id = func_call.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let actual_id = if !call_id.is_empty() {
                call_id.to_string()
            } else {
                format!("call_{}", func_call.get("name").and_then(|v| v.as_str()).unwrap_or("unknown"))
            };

            let func_name = func_call.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let func_args = func_call.get("args").cloned().unwrap_or(Json::Object(Default::default()));

            tool_calls.push(serde_json::json!({
                "id": actual_id,
                "type": openai_blocks::FUNCTION,
                "function": {
                    "name": func_name,
                    "arguments": serde_json::to_string(&func_args).unwrap_or_default()
                }
            }));
        }

        // Function response
        if let Some(func_resp) = part.get("functionResponse").and_then(|v| v.as_object()) {
            let resp_id = func_resp.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let actual_id = if !resp_id.is_empty() {
                resp_id.to_string()
            } else {
                format!("call_{}", func_resp.get("name").and_then(|v| v.as_str()).unwrap_or("unknown"))
            };

            let response = func_resp.get("response").cloned().unwrap_or(Json::Object(Default::default()));
            let content_val = response.get("result").cloned().unwrap_or_else(|| response.clone());

            tool_results.push(serde_json::json!({
                "role": roles::TOOL,
                "tool_call_id": actual_id,
                "content": serde_json::to_string(&content_val).unwrap_or_default()
            }));
        }
    }

    let message = if !tool_calls.is_empty() {
        let mut msg = serde_json::json!({
            "role": roles::ASSISTANT,
            "tool_calls": tool_calls
        });
        if !text_parts.is_empty() {
            msg["content"] = if text_parts.len() == 1 {
                text_parts[0].get("text").cloned().unwrap_or(Json::Null)
            } else {
                Json::Array(text_parts)
            };
        }
        msg
    } else if !text_parts.is_empty() {
        serde_json::json!({
            "role": role,
            "content": if text_parts.len() == 1 {
                text_parts[0].get("text").cloned().unwrap_or(Json::Null)
            } else {
                Json::Array(text_parts)
            }
        })
    } else {
        return None;
    };

    // Return tool results first, then the main message
    if !tool_results.is_empty() {
        let mut all_msgs = tool_results;
        all_msgs.push(message);
        Some(Json::Array(all_msgs))
    } else {
        Some(message)
    }
}

/// Extract text from Gemini content.
fn extract_gemini_text(content: &Json) -> Option<String> {
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }

    if let Some(parts) = content.get("parts").and_then(|v| v.as_array()) {
        let text_parts: Vec<String> = parts.iter()
            .filter_map(|p| p.get("text").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .collect();
        return Some(text_parts.join(""));
    }

    None
}

/// Adjust max_tokens based on request context.
/// Constants: DEFAULT_MAX_TOKENS = 64000, DEFAULT_MIN_TOKENS = 32000
fn adjust_max_tokens(body: &Json) -> u64 {
    const DEFAULT_MAX_TOKENS: u64 = 64000;
    const DEFAULT_MIN_TOKENS: u64 = 32000;

    let mut max_tokens = body.get("max_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_MAX_TOKENS);

    // Auto-increase for tool calling
    if body.get("tools").and_then(|v| v.as_array()).map_or(false, |t| !t.is_empty()) {
        if max_tokens < DEFAULT_MIN_TOKENS {
            max_tokens = DEFAULT_MIN_TOKENS;
        }
    }

    // Ensure max_tokens > thinking.budget_tokens
    if let Some(thinking) = body.get("thinking").and_then(|v| v.as_object()) {
        if let Some(budget) = thinking.get("budget_tokens").and_then(|v| v.as_u64()) {
            if max_tokens <= budget {
                max_tokens = budget + 1024;
            }
        }
    }

    // Never exceed ceiling
    if max_tokens > DEFAULT_MAX_TOKENS {
        max_tokens = DEFAULT_MAX_TOKENS;
    }

    max_tokens
}
