//! Claude to OpenAI request translator. Port of open-sse/translator/request/claude-to-openai.js.

use nr_core::{Credentials, Json};
use crate::{TranslationOptions, schema::*};

pub fn claude_to_openai_request(
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

    // Max tokens
    if body.get("max_tokens").is_some() {
        let max_tokens = adjust_max_tokens(&body);
        result["max_tokens"] = Json::Number(max_tokens.into());
    }

    // Temperature
    if let Some(temp) = body.get("temperature").and_then(|v| v.as_f64()) {
        result["temperature"] = serde_json::json!(temp);
    }

    // System message
    if let Some(system) = body.get("system") {
        let system_content = if let Some(arr) = system.as_array() {
            let parts: Vec<String> = arr.iter()
                .filter_map(|s| s.get("text").and_then(|v| v.as_str()).map(String::from))
                .collect();
            parts.join("\n")
        } else if let Some(s) = system.as_str() {
            s.to_string()
        } else {
            String::new()
        };

        if !system_content.is_empty() {
            result["messages"].as_array_mut().unwrap().push(serde_json::json!({
                "role": roles::SYSTEM,
                "content": system_content
            }));
        }
    }

    // Convert messages
    if let Some(messages) = body.get("messages").and_then(|v| v.as_array()) {
        for msg in messages {
            if let Some(converted) = convert_claude_message(msg) {
                match converted {
                    Json::Array(msgs) => {
                        for m in msgs {
                            result["messages"].as_array_mut().unwrap().push(m);
                        }
                    }
                    _ => {
                        result["messages"].as_array_mut().unwrap().push(converted);
                    }
                }
            }
        }
    }

    // Fix missing tool responses
    fix_missing_tool_responses(&mut result["messages"]);

    // Tools
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        let tool_list: Vec<Json> = tools.iter().map(|tool| {
            serde_json::json!({
                "type": openai_blocks::FUNCTION,
                "function": {
                    "name": tool.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                    "description": tool.get("description").and_then(|v| v.as_str()).unwrap_or(""),
                    "parameters": tool.get("input_schema").cloned().unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}}))
                }
            })
        }).collect();
        if !tool_list.is_empty() {
            result["tools"] = Json::Array(tool_list);
        }
    }

    // Tool choice
    if let Some(tc) = body.get("tool_choice") {
        result["tool_choice"] = convert_tool_choice_to_openai(tc);
    }

    // Reasoning effort
    if let Some(reasoning_effort) = body.get("reasoning_effort").and_then(|v| v.as_str()) {
        result["reasoning_effort"] = Json::String(reasoning_effort.to_string());
    }

    result
}

fn convert_claude_message(msg: &Json) -> Option<Json> {
    let role = msg.get("role").and_then(|v| v.as_str())?;

    if role == roles::SYSTEM {
        // Convert system to user message with directive tags
        if let Some(content) = msg.get("content") {
            let text = if let Some(s) = content.as_str() {
                format!("<instructions>\n{}\n</instructions>", s)
            } else {
                return None;
            };
            return Some(serde_json::json!({"role": roles::USER, "content": text}));
        }
    }

    let content = msg.get("content")?;
    let role = if matches!(role, roles::USER | roles::TOOL) { roles::USER } else { roles::ASSISTANT };

    if let Some(s) = content.as_str() {
        return Some(serde_json::json!({"role": role, "content": s}));
    }

    if let Some(arr) = content.as_array() {
        let mut parts = Vec::new();
        let mut tool_calls = Vec::new();
        let mut tool_results = Vec::new();

        for block in arr {
            if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                parts.push(serde_json::json!({"type": openai_blocks::TEXT, "text": text}));
            }

            if let Some(image) = block.get("image") {
                if let Some(source) = image.get("source") {
                    // Handle Claude image source
                    if let Some(data) = source.get("data").and_then(|v| v.as_str()) {
                        parts.push(serde_json::json!({
                            "type": openai_blocks::IMAGE_URL,
                            "image_url": {"url": format!("data:image/png;base64,{}", data)}
                        }));
                    }
                }
            }

            if let Some(tool_use) = block.get("tool_use") {
                let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let name = tool_use.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let input = tool_use.get("input").cloned().unwrap_or(serde_json::json!({}));

                tool_calls.push(serde_json::json!({
                    "id": id,
                    "type": openai_blocks::FUNCTION,
                    "function": {"name": name, "arguments": serde_json::to_string(&input).unwrap_or_default()}
                }));
            }

            if let Some(tool_result) = block.get("tool_result") {
                let tool_use_id = block.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
                tool_results.push(serde_json::json!({
                    "role": roles::TOOL,
                    "tool_call_id": tool_use_id,
                    "content": tool_result.get("content").cloned().unwrap_or(Json::Null)
                }));
            }
        }

        if !tool_calls.is_empty() {
            let mut result = serde_json::json!({"role": roles::ASSISTANT});
            if !parts.is_empty() {
                result["content"] = if parts.len() == 1 {
                    parts[0].get("text").cloned().unwrap_or(Json::Null)
                } else {
                    Json::Array(parts)
                };
            }
            result["tool_calls"] = Json::Array(tool_calls);
            return if !tool_results.is_empty() {
                let mut all = tool_results;
                all.push(result);
                Some(Json::Array(all))
            } else {
                Some(result)
            };
        }

        if !parts.is_empty() {
            let content = if parts.len() == 1 {
                parts[0].get("text").cloned().unwrap_or(Json::Null)
            } else {
                Json::Array(parts)
            };
            return Some(serde_json::json!({"role": role, "content": content}));
        }

        if !tool_results.is_empty() {
            return Some(if tool_results.len() == 1 {
                tool_results.into_iter().next().unwrap()
            } else {
                Json::Array(tool_results)
            });
        }
    }

    None
}

fn fix_missing_tool_responses(messages: &mut Json) {
    if let Some(msg_arr) = messages.as_array_mut() {
        for i in 0..msg_arr.len() {
            if msg_arr[i].get("role").and_then(|v| v.as_str()) == Some(roles::ASSISTANT) {
                if let Some(tool_calls) = msg_arr[i].get("tool_calls").and_then(|v| v.as_array()) {
                    let call_ids: Vec<String> = tool_calls.iter()
                        .filter_map(|tc| tc.get("id").and_then(|v| v.as_str()).map(String::from))
                        .collect();

                    let mut responded_ids = std::collections::HashSet::new();
                    let mut insert_pos = i + 1;

                    for j in (i + 1)..msg_arr.len() {
                        if msg_arr[j].get("role").and_then(|v| v.as_str()) == Some(roles::TOOL) {
                            if let Some(id) = msg_arr[j].get("tool_call_id").and_then(|v| v.as_str()) {
                                responded_ids.insert(id.to_string());
                                insert_pos = j + 1;
                            }
                        } else {
                            break;
                        }
                    }

                    let missing: Vec<_> = call_ids.iter()
                        .filter(|id| !responded_ids.contains(*id))
                        .collect();

                    if !missing.is_empty() {
                        for mid in missing {
                            msg_arr.insert(insert_pos, serde_json::json!({
                                "role": roles::TOOL,
                                "tool_call_id": mid,
                                "content": "[No response received]"
                            }));
                            insert_pos += 1;
                        }
                    }
                }
            }
        }
    }
}

fn convert_tool_choice_to_openai(choice: &Json) -> Json {
    if let Some(s) = choice.as_str() {
        return serde_json::json!(s);
    }

    if let Some(obj) = choice.as_object() {
        if let Some(t) = obj.get("type").and_then(|v| v.as_str()) {
            match t {
                "auto" | "any" => return serde_json::json!("auto"),
                "tool" => {
                    if let Some(name) = obj.get("name").and_then(|v| v.as_str()) {
                        return serde_json::json!({"type": "function", "function": {"name": name}});
                    }
                }
                _ => {}
            }
        }
    }

    serde_json::json!("auto")
}

fn adjust_max_tokens(body: &Json) -> u64 {
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

    if max_tokens > DEFAULT_MAX_TOKENS {
        max_tokens = DEFAULT_MAX_TOKENS;
    }

    max_tokens
}
