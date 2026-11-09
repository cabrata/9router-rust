//! OpenAI to Claude request translator. Port of open-sse/translator/request/openai-to-claude.js.

use nr_core::{Credentials, Json};
use crate::{TranslationOptions, schema::*};

const CLAUDE_OAUTH_TOOL_PREFIX: &str = "";

/// Convert OpenAI request to Claude format.
pub fn openai_to_claude_request(
    model: &str,
    body: Json,
    stream: bool,
    _credentials: Option<&mut Credentials>,
    _options: &TranslationOptions,
) -> Json {
    let mut result = serde_json::json!({
        "model": model,
        "stream": stream
    });

    // Max tokens - use model ceiling if available
    let model_ceiling = 128000_u64;  // TODO(dedupe): use getCapabilitiesForModel
    let max_tokens = adjust_max_tokens_for_model(&body, Some(model_ceiling));
    result["max_tokens"] = Json::Number(max_tokens.into());

    // Temperature
    if let Some(temp) = body.get("temperature").and_then(|v| v.as_f64()) {
        result["temperature"] = Json::Number(serde_json::Number::from_f64(temp).unwrap_or_default());
    }

    // Messages
    let mut messages = Vec::new();
    let mut system_parts = Vec::new();

    if let Some(msg_arr) = body.get("messages").and_then(|v| v.as_array()) {
        // Extract system messages
        for msg in msg_arr {
            if msg.get("role").and_then(|v| v.as_str()) == Some(roles::SYSTEM) {
                if let Some(content) = msg.get("content") {
                    if let Some(text) = content.as_str() {
                        system_parts.push(text.to_string());
                    }
                }
            }
        }

        // Process non-system messages
        let non_system: Vec<_> = msg_arr.iter()
            .filter(|m| m.get("role").and_then(|v| v.as_str()) != Some(roles::SYSTEM))
            .collect();

        let mut current_role: Option<&str> = None;
        let mut current_parts: Vec<Json> = Vec::new();

        let flush_message = |messages: &mut Vec<Json>, role: Option<&str>, parts: &mut Vec<Json>| {
            if let Some(r) = role {
                if !parts.is_empty() {
                    messages.push(serde_json::json!({
                        "role": r,
                        "content": parts.clone()
                    }));
                    parts.clear();
                }
            }
        };

        for msg in non_system {
            let msg_role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
            let new_role = if msg_role == roles::USER || msg_role == roles::TOOL {
                roles::USER
            } else {
                roles::ASSISTANT
            };

            let blocks = get_content_blocks_from_message(msg);
            let has_tool_use = blocks.iter().any(|b| b.get("type").and_then(|v| v.as_str()) == Some(claude_blocks::TOOL_USE));
            let has_tool_result = blocks.iter().any(|b| b.get("type").and_then(|v| v.as_str()) == Some(claude_blocks::TOOL_RESULT));

            // Handle tool results specially
            if has_tool_result {
                let tool_result_blocks: Vec<_> = blocks.iter().filter(|b| b.get("type").and_then(|v| v.as_str()) == Some(claude_blocks::TOOL_RESULT)).cloned().collect();
                let other_blocks: Vec<_> = blocks.iter().filter(|b| b.get("type").and_then(|v| v.as_str()) != Some(claude_blocks::TOOL_RESULT)).cloned().collect();

                flush_message(&mut messages, current_role, &mut current_parts);
                current_role = None;

                if !tool_result_blocks.is_empty() {
                    messages.push(serde_json::json!({
                        "role": roles::USER,
                        "content": tool_result_blocks
                    }));
                }

                if !other_blocks.is_empty() {
                    current_role = Some(new_role);
                    current_parts.extend(other_blocks);
                }
                continue;
            }

            if current_role != Some(new_role) {
                flush_message(&mut messages, current_role, &mut current_parts);
                current_role = Some(new_role);
            }

            current_parts.extend(blocks.clone());

            if has_tool_use {
                flush_message(&mut messages, current_role, &mut current_parts);
                current_role = None;
            }
        }

        flush_message(&mut messages, current_role, &mut current_parts);
    }

    // Add cache_control to last assistant message
    for i in (0..messages.len()).rev() {
        if messages[i].get("role").and_then(|v| v.as_str()) == Some(roles::ASSISTANT) {
            if let Some(content) = messages[i].get_mut("content").and_then(|v| v.as_array_mut()) {
                for block in content.iter_mut().rev() {
                    if let Some(block_obj) = block.as_object_mut() {
                        if let Some(block_type) = block_obj.get("type").and_then(|v| v.as_str()) {
                            if matches!(block_type, claude_blocks::TEXT | claude_blocks::TOOL_USE | claude_blocks::TOOL_RESULT | claude_blocks::IMAGE) {
                                block_obj.insert("cache_control".to_string(), serde_json::json!({"type": "ephemeral"}));
                                break;
                            }
                        }
                    }
                }
            }
            break;
        }
    }

    result["messages"] = Json::Array(messages);

    // System message with cache control
    let claude_system_prompt = r#"You are Claude Code, an expert programmer. You help with coding tasks."#;  // TODO(dedupe): use CLAUDE_SYSTEM_PROMPT from appConstants

    let mut system = vec![serde_json::json!({
        "type": claude_blocks::TEXT,
        "text": claude_system_prompt
    })];

    if !system_parts.is_empty() {
        system.push(serde_json::json!({
            "type": claude_blocks::TEXT,
            "text": system_parts.join("\n"),
            "cache_control": {"type": "ephemeral", "ttl": "1h"}
        }));
    }

    result["system"] = Json::Array(system);

    // Tools
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        let mut tool_list = Vec::new();

        for tool in tools {
            // Pass through non-function tools
            if let Some(tool_type) = tool.get("type").and_then(|v| v.as_str()) {
                if tool_type != openai_blocks::FUNCTION {
                    tool_list.push(tool.clone());
                    continue;
                }
            }

            let tool_data = tool.get("function").cloned().unwrap_or_else(|| tool.clone());
            let name = tool_data.get("name").and_then(|v| v.as_str()).unwrap_or("unknown");
            let desc = tool_data.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let params = tool_data.get("parameters").cloned().or_else(|| tool_data.get("input_schema").cloned())
                .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}, "required": []}));

            tool_list.push(serde_json::json!({
                "name": name,
                "description": desc,
                "input_schema": params
            }));
        }

        if !tool_list.is_empty() {
            if let Some(last) = tool_list.last_mut() {
                if let Some(obj) = last.as_object_mut() {
                    obj.insert("cache_control".to_string(), serde_json::json!({"type": "ephemeral", "ttl": "1h"}));
                }
            }
            result["tools"] = Json::Array(tool_list);
        }
    }

    // Tool choice
    if let Some(tool_choice) = body.get("tool_choice") {
        result["tool_choice"] = convert_openai_tool_choice(tool_choice);
    }

    result
}

/// Get content blocks from a single OpenAI message, converting to Claude format.
fn get_content_blocks_from_message(msg: &Json) -> Vec<Json> {
    let mut blocks = Vec::new();
    let msg_role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");

    if msg_role == roles::TOOL {
        if let Some(tool_call_id) = msg.get("tool_call_id").and_then(|v| v.as_str()) {
            let content = msg.get("content").cloned().unwrap_or(Json::Null);
            blocks.push(serde_json::json!({
                "type": claude_blocks::TOOL_RESULT,
                "tool_use_id": tool_call_id,
                "content": content
            }));
        }
    } else if msg_role == roles::USER {
        if let Some(content_str) = msg.get("content").and_then(|v| v.as_str()) {
            if !content_str.is_empty() {
                blocks.push(serde_json::json!({
                    "type": claude_blocks::TEXT,
                    "text": content_str
                }));
            }
        } else if let Some(content_arr) = msg.get("content").and_then(|v| v.as_array()) {
            for part in content_arr {
                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        blocks.push(serde_json::json!({
                            "type": claude_blocks::TEXT,
                            "text": text
                        }));
                    }
                }
                // Handle image_url
                if let Some(image_url) = part.get("image_url").and_then(|v| v.get("url")).and_then(|v| v.as_str()) {
                    // TODO: Parse data URIs and convert
                    blocks.push(serde_json::json!({
                        "type": claude_blocks::IMAGE,
                        "source": {"type": "url", "url": image_url}
                    }));
                }
            }
        }
    } else if msg_role == roles::ASSISTANT {
        // Handle text content
        if let Some(content_str) = msg.get("content").and_then(|v| v.as_str()) {
            if !content_str.is_empty() {
                blocks.push(serde_json::json!({
                    "type": claude_blocks::TEXT,
                    "text": content_str
                }));
            }
        } else if let Some(content_arr) = msg.get("content").and_then(|v| v.as_array()) {
            for part in content_arr {
                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                    blocks.push(serde_json::json!({
                        "type": claude_blocks::TEXT,
                        "text": text
                    }));
                }
            }
        }

        // Handle tool_calls
        if let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
            for tc in tool_calls {
                if tc.get("type").and_then(|v| v.as_str()) == Some(openai_blocks::FUNCTION) {
                    if let Some(func) = tc.get("function") {
                        let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");
                        let name = func.get("name").and_then(|v| v.as_str()).unwrap_or("unknown");
                        let args_str = func.get("arguments").and_then(|v| v.as_str()).unwrap_or("{}");
                        let input = serde_json::from_str(args_str).unwrap_or(serde_json::json!({}));

                        blocks.push(serde_json::json!({
                            "type": claude_blocks::TOOL_USE,
                            "id": id,
                            "name": name,
                            "input": input
                        }));
                    }
                }
            }
        }
    }

    blocks
}

/// Convert OpenAI tool_choice to Claude format.
fn convert_openai_tool_choice(choice: &Json) -> Json {
    if let Some(s) = choice.as_str() {
        if s == "required" {
            return serde_json::json!({"type": "any"});
        }
        return serde_json::json!({"type": "auto"});
    }

    if let Some(obj) = choice.as_object() {
        if let Some(func_name) = obj.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str()) {
            return serde_json::json!({"type": "tool", "name": func_name});
        }
        if let Some(choice_type) = obj.get("type").and_then(|v| v.as_str()) {
            if matches!(choice_type, "auto" | "any" | "tool" | "none") {
                return choice.clone();
            }
        }
    }

    serde_json::json!({"type": "auto"})
}

/// Adjust max_tokens based on model ceiling.
fn adjust_max_tokens_for_model(body: &Json, ceiling: Option<u64>) -> u64 {
    const DEFAULT_MAX_TOKENS: u64 = 64000;
    const DEFAULT_MIN_TOKENS: u64 = 32000;

    let mut max_tokens = body.get("max_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_MAX_TOKENS);

    if body.get("tools").and_then(|v| v.as_array()).map_or(false, |t| !t.is_empty()) {
        if max_tokens < DEFAULT_MIN_TOKENS {
            max_tokens = DEFAULT_MIN_TOKENS;
        }
    }

    if let Some(thinking) = body.get("thinking").and_then(|v| v.as_object()) {
        if let Some(budget) = thinking.get("budget_tokens").and_then(|v| v.as_u64()) {
            if max_tokens <= budget {
                max_tokens = budget + 1024;
            }
        }
    }

    let actual_ceiling = ceiling.unwrap_or(DEFAULT_MAX_TOKENS);
    if max_tokens > actual_ceiling {
        max_tokens = actual_ceiling;
    }

    max_tokens
}
