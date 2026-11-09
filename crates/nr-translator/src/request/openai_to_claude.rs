//! OpenAI to Claude request translator. Port of open-sse/translator/request/openai-to-claude.js.

use nr_core::{Credentials, Json};
use serde_json::json;

use crate::concerns::image::parse_data_uri;
use crate::concerns::json::safe_parse_json;
use crate::formats::gemini::extract_text_content;
use crate::formats::max_tokens::adjust_max_tokens;
use crate::schema::{claude_blocks, openai_blocks, roles};
use crate::TranslationOptions;

// Empty prefix matches real Claude Code behavior (no tool name prefix).
// Previously "proxy_" was used but this is a detectable fingerprint difference.
const CLAUDE_OAUTH_TOOL_PREFIX: &str = "";

fn claude_system_prompt() -> &'static str {
    static PROMPT: once_cell::sync::Lazy<String> = once_cell::sync::Lazy::new(|| {
        nr_data::export("open-sse/config/appConstants", "CLAUDE_SYSTEM_PROMPT")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "You are Claude Code, Anthropic's official CLI for Claude.".to_string())
    });
    PROMPT.as_str()
}

/// Convert OpenAI request to Claude format.
pub fn openai_to_claude_request(
    model: &str,
    body: Json,
    stream: bool,
    _credentials: Option<&mut Credentials>,
    _options: &TranslationOptions,
) -> Json {
    let mut name_map: Vec<(String, String)> = Vec::new();

    // Cap max_tokens at the model's real output ceiling (e.g. Opus 4.8 = 128000).
    let model_ceiling = nr_data::capabilities::get_capabilities_for_model(None, Some(model))
        .get("maxOutput")
        .and_then(|v| v.as_i64());

    let mut result = json!({
        "model": model,
        "max_tokens": adjust_max_tokens(&body, model_ceiling),
        "stream": stream
    });

    if let Some(temp) = body.get("temperature").filter(|v| !v.is_null()) {
        result["temperature"] = temp.clone();
    }

    // Messages
    let mut messages: Vec<Json> = Vec::new();
    let mut system_parts: Vec<String> = Vec::new();

    if let Some(msg_arr) = body.get("messages").and_then(|v| v.as_array()) {
        // Extract system messages
        for msg in msg_arr {
            if msg.get("role").and_then(|v| v.as_str()) == Some(roles::SYSTEM) {
                let content = msg.get("content").cloned().unwrap_or(Json::Null);
                system_parts.push(if let Some(s) = content.as_str() {
                    s.to_string()
                } else {
                    extract_text_content(&content, "\n")
                });
            }
        }

        // Process messages with merging logic.
        // CRITICAL: tool_result must be in separate message immediately after tool_use.
        let mut current_role: Option<String> = None;
        let mut current_parts: Vec<Json> = Vec::new();

        for msg in msg_arr.iter().filter(|m| m.get("role").and_then(|v| v.as_str()) != Some(roles::SYSTEM)) {
            let msg_role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
            let new_role = if msg_role == roles::USER || msg_role == roles::TOOL { roles::USER } else { roles::ASSISTANT };
            let blocks = get_content_blocks_from_message(msg);
            let has_tool_use = blocks.iter().any(|b| b.get("type").and_then(|v| v.as_str()) == Some(claude_blocks::TOOL_USE));
            let has_tool_result = blocks.iter().any(|b| b.get("type").and_then(|v| v.as_str()) == Some(claude_blocks::TOOL_RESULT));

            let flush = |messages: &mut Vec<Json>, role: &mut Option<String>, parts: &mut Vec<Json>| {
                if let Some(r) = role.take() {
                    if !parts.is_empty() {
                        messages.push(json!({"role": r, "content": std::mem::take(parts)}));
                    }
                }
            };

            // Separate tool_result from other content
            if has_tool_result {
                let tool_result_blocks: Vec<Json> = blocks.iter().filter(|b| b.get("type").and_then(|v| v.as_str()) == Some(claude_blocks::TOOL_RESULT)).cloned().collect();
                let other_blocks: Vec<Json> = blocks.iter().filter(|b| b.get("type").and_then(|v| v.as_str()) != Some(claude_blocks::TOOL_RESULT)).cloned().collect();

                flush(&mut messages, &mut current_role, &mut current_parts);

                if !tool_result_blocks.is_empty() {
                    messages.push(json!({"role": roles::USER, "content": tool_result_blocks}));
                }
                if !other_blocks.is_empty() {
                    current_role = Some(new_role.to_string());
                    current_parts.extend(other_blocks);
                }
                continue;
            }

            if current_role.as_deref() != Some(new_role) {
                flush(&mut messages, &mut current_role, &mut current_parts);
                current_role = Some(new_role.to_string());
            }

            current_parts.extend(blocks);

            if has_tool_use {
                flush(&mut messages, &mut current_role, &mut current_parts);
            }
        }

        // Final flush
        if let Some(r) = current_role.take() {
            if !current_parts.is_empty() {
                messages.push(json!({"role": r, "content": current_parts}));
            }
        }

        // Add cache_control to last assistant message
        for i in (0..messages.len()).rev() {
            if messages[i].get("role").and_then(|v| v.as_str()) == Some(roles::ASSISTANT) {
                if let Some(content) = messages[i].get_mut("content").and_then(|v| v.as_array_mut()) {
                    if content.is_empty() {
                        continue;
                    }
                    let valid = [claude_blocks::TEXT, claude_blocks::TOOL_USE, claude_blocks::TOOL_RESULT, claude_blocks::IMAGE];
                    for block in content.iter_mut().rev() {
                        let t = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        if valid.contains(&t) {
                            block["cache_control"] = json!({"type": "ephemeral"});
                            break;
                        }
                    }
                    break;
                }
            }
        }
    }

    result["messages"] = Json::Array(messages);

    // Handle response_format for JSON mode
    if let Some(rf) = body.get("response_format").filter(|v| v.is_object()) {
        let rf_type = rf.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if rf_type == "json_schema" {
            if let Some(schema) = rf.get("json_schema").and_then(|j| j.get("schema")) {
                let schema_json = serde_json::to_string_pretty(schema).unwrap_or_default();
                system_parts.push(format!(
                    "You must respond with valid JSON that strictly follows this JSON schema:\n```json\n{schema_json}\n```\nRespond ONLY with the JSON object, no other text."
                ));
            }
        } else if rf_type == "json_object" {
            system_parts.push("You must respond with valid JSON. Respond ONLY with a JSON object, no other text.".to_string());
        }
    }

    // System with Claude Code prompt and cache_control
    let claude_code_prompt = json!({"type": claude_blocks::TEXT, "text": claude_system_prompt()});
    if system_parts.is_empty() {
        result["system"] = json!([claude_code_prompt]);
    } else {
        let system_text = system_parts.join("\n");
        result["system"] = json!([
            claude_code_prompt,
            {"type": claude_blocks::TEXT, "text": system_text, "cache_control": {"type": "ephemeral", "ttl": "1h"}}
        ]);
    }

    // Tools
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        let mut converted: Vec<Json> = Vec::new();
        for tool in tools {
            let tool_type = tool.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if !tool_type.is_empty() && tool_type != openai_blocks::FUNCTION {
                converted.push(tool.clone());
                continue;
            }
            // Function-shaped tools arrive in two flavors from real clients:
            //   (a) openai-spec: { type: "function", function: { name, ... } }
            //   (b) legacy/loose: { function: { name, ... } } (no parent `type`)
            let tool_data = tool.get("function").filter(|f| f.is_object()).unwrap_or(tool);
            let original_name = tool_data.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let tool_name = format!("{CLAUDE_OAUTH_TOOL_PREFIX}{original_name}");
            name_map.push((tool_name.clone(), original_name));

            let params = tool_data
                .get("parameters")
                .or_else(|| tool_data.get("input_schema"))
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}, "required": []}));
            converted.push(json!({
                "name": tool_name,
                "description": tool_data.get("description").and_then(|v| v.as_str()).unwrap_or(""),
                "input_schema": params
            }));
        }
        if let Some(last) = converted.last_mut() {
            last["cache_control"] = json!({"type": "ephemeral", "ttl": "1h"});
        }
        if !converted.is_empty() {
            result["tools"] = Json::Array(converted);
        }
    }

    if let Some(choice) = body.get("tool_choice").filter(|v| !v.is_null()) {
        result["tool_choice"] = convert_openai_tool_choice(choice);
    }

    // Thinking is normalized centrally by apply_thinking after translation.
    if !name_map.is_empty() {
        let mut map = serde_json::Map::new();
        // `_toolNameMap` is attached for response translation. HashMap shape is not
        // part of the wire body (server strips `_`-prefixed before dispatch).
        let mut combined: serde_json::Map<String, Json> = serde_json::Map::new();
        for (k, v) in name_map {
            combined.insert(k, json!(v));
        }
        map.insert("_toolNameMap".to_string(), Json::Object(combined));
        if let Some(obj) = result.as_object_mut() {
            obj.extend(map);
        }
    }

    result
}

/// Claude only accepts tool_choice.type of "auto" | "any" | "tool" | "none";
/// anything else (e.g. OpenAI's "function") triggers a 400.
fn convert_openai_tool_choice(choice: &Json) -> Json {
    if let Some(s) = choice.as_str() {
        return if s == "required" { json!({"type": "any"}) } else { json!({"type": "auto"}) };
    }
    if choice.is_object() {
        if let Some(name) = choice.get("function").and_then(|f| f.get("name")).filter(|v| v.is_string()) {
            return json!({"type": "tool", "name": name});
        }
        if let Some(t) = choice.get("type").and_then(|v| v.as_str()) {
            if matches!(t, "auto" | "any" | "tool" | "none") {
                return choice.clone();
            }
        }
    }
    json!({"type": "auto"})
}

/// A tool message may carry an OpenAI content array (text + image_url parts).
/// Claude tool_result content accepts image blocks, but not OpenAI image_url parts.
fn convert_tool_result_content(content: &Json) -> Json {
    let Some(arr) = content.as_array() else { return content.clone() };
    let mut parts: Vec<Json> = Vec::new();
    for part in arr {
        let t = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if t == openai_blocks::TEXT {
            if let Some(text) = part.get("text").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                parts.push(json!({"type": claude_blocks::TEXT, "text": text}));
            }
        } else if t == openai_blocks::IMAGE_URL {
            let url = part.get("image_url").and_then(|u| u.as_str().or_else(|| u.get("url").and_then(|x| x.as_str()))).unwrap_or("");
            push_image(url, &mut parts);
        } else if t == claude_blocks::IMAGE {
            if let Some(source) = part.get("source") {
                parts.push(json!({"type": claude_blocks::IMAGE, "source": source}));
            }
        }
    }
    if parts.is_empty() { content.clone() } else { Json::Array(parts) }
}

fn push_image(url: &str, parts: &mut Vec<Json>) {
    if let Some(parsed) = parse_data_uri(url) {
        parts.push(json!({
            "type": claude_blocks::IMAGE,
            "source": {"type": "base64", "media_type": parsed.mime_type, "data": parsed.base64}
        }));
    } else if url.starts_with("http://") || url.starts_with("https://") {
        parts.push(json!({
            "type": claude_blocks::IMAGE,
            "source": {"type": "url", "url": url}
        }));
    }
}

fn get_content_blocks_from_message(msg: &Json) -> Vec<Json> {
    let mut blocks: Vec<Json> = Vec::new();
    let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");

    if role == roles::TOOL {
        blocks.push(json!({
            "type": claude_blocks::TOOL_RESULT,
            "tool_use_id": msg.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or(""),
            "content": convert_tool_result_content(&msg.get("content").cloned().unwrap_or(Json::Null))
        }));
    } else if role == roles::USER {
        match msg.get("content") {
            Some(Json::String(s)) if !s.is_empty() => {
                blocks.push(json!({"type": claude_blocks::TEXT, "text": s}));
            }
            Some(Json::Array(parts)) => {
                for part in parts {
                    let t = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if t == openai_blocks::TEXT {
                        if let Some(text) = part.get("text").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                            blocks.push(json!({"type": claude_blocks::TEXT, "text": text}));
                        }
                    } else if t == claude_blocks::TOOL_RESULT {
                        let mut block = json!({
                            "type": claude_blocks::TOOL_RESULT,
                            "tool_use_id": part.get("tool_use_id").cloned().unwrap_or(Json::Null),
                            "content": part.get("content").cloned().unwrap_or(Json::Null)
                        });
                        if part.get("is_error").map(nr_core::json::truthy).unwrap_or(false) {
                            block["is_error"] = part["is_error"].clone();
                        }
                        blocks.push(block);
                    } else if t == openai_blocks::IMAGE_URL {
                        let url = part.get("image_url").and_then(|u| u.as_str().or_else(|| u.get("url").and_then(|x| x.as_str()))).unwrap_or("");
                        push_image(url, &mut blocks);
                    } else if t == openai_blocks::IMAGE {
                        if let Some(source) = part.get("source") {
                            blocks.push(json!({"type": claude_blocks::IMAGE, "source": source}));
                        }
                    } else if t == openai_blocks::FILE {
                        // OpenAI file block -> Claude document (PDF only; Claude rejects other mimes).
                        let file_data = part.get("file").and_then(|f| f.get("file_data")).and_then(|v| v.as_str()).unwrap_or("");
                        if let Some(parsed) = parse_data_uri(file_data) {
                            if parsed.mime_type == "application/pdf" {
                                blocks.push(json!({
                                    "type": claude_blocks::DOCUMENT,
                                    "source": {"type": "base64", "media_type": parsed.mime_type, "data": parsed.base64}
                                }));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    } else if role == roles::ASSISTANT {
        match msg.get("content") {
            Some(Json::Array(parts)) => {
                for part in parts {
                    let t = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if t == openai_blocks::TEXT {
                        if let Some(text) = part.get("text").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                            blocks.push(json!({"type": claude_blocks::TEXT, "text": text}));
                        }
                    } else if t == claude_blocks::TOOL_USE {
                        blocks.push(json!({
                            "type": claude_blocks::TOOL_USE,
                            "id": part.get("id").cloned().unwrap_or(Json::Null),
                            "name": part.get("name").cloned().unwrap_or(Json::Null),
                            "input": part.get("input").cloned().unwrap_or(Json::Null)
                        }));
                    } else if t == claude_blocks::THINKING {
                        // Include thinking block but strip cache_control (not allowed on thinking blocks).
                        let mut thinking = part.clone();
                        if let Some(obj) = thinking.as_object_mut() {
                            obj.remove("cache_control");
                        }
                        blocks.push(thinking);
                    }
                }
            }
            _ => {
                let text = match msg.get("content") {
                    Some(Json::String(s)) => s.clone(),
                    Some(other) => extract_text_content(other, "\n"),
                    None => String::new(),
                };
                if !text.is_empty() {
                    blocks.push(json!({"type": claude_blocks::TEXT, "text": text}));
                }
            }
        }
        if let Some(tool_calls) = msg.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in tool_calls {
                if tc.get("type").and_then(|v| v.as_str()) != Some(openai_blocks::FUNCTION) {
                    continue;
                }
                let func = tc.get("function").cloned().unwrap_or(Json::Null);
                let name = func.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args = func.get("arguments").cloned().unwrap_or(Json::Null);
                let input = match args.as_str() {
                    Some(s) => safe_parse_json(s, json!(s)),
                    None => args.clone(),
                };
                blocks.push(json!({
                    "type": claude_blocks::TOOL_USE,
                    "id": tc.get("id").cloned().unwrap_or(Json::Null),
                    "name": format!("{CLAUDE_OAUTH_TOOL_PREFIX}{name}"),
                    "input": input
                }));
            }
        }
    }

    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_openai_to_claude_basic() {
        let body = json!({
            "messages": [
                {"role": "system", "content": "Be concise."},
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "hi"},
                {"role": "user", "content": "ok"}
            ],
            "temperature": 0.2
        });
        let result = openai_to_claude_request("claude-sonnet-4-5", body, true, None, &Default::default());
        assert_eq!(result["stream"], true);
        assert_eq!(result["temperature"], 0.2);
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["text"], "hello");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["cache_control"]["type"], "ephemeral");
        let system = result["system"].as_array().unwrap();
        assert_eq!(system.len(), 2);
        assert!(system[0]["text"].as_str().unwrap().contains("Claude Code"));
        assert_eq!(system[1]["text"], "Be concise.");
    }

    #[test]
    fn test_openai_to_claude_tool_calls_and_results() {
        let body = json!({
            "messages": [
                {"role": "user", "content": "search"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "c1", "type": "function", "function": {"name": "search", "arguments": "{\"q\":\"x\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "c1", "content": "found"}
            ],
            "tools": [{"type": "function", "function": {"name": "search", "description": "d", "parameters": {"type": "object"}}}]
        });
        let result = openai_to_claude_request("claude-sonnet-4-5", body, true, None, &Default::default());
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["content"][0]["type"], "tool_use");
        assert_eq!(messages[1]["content"][0]["input"]["q"], "x");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "c1");
        assert_eq!(result["tools"][0]["name"], "search");
        assert_eq!(result["tools"][0]["cache_control"]["ttl"], "1h");
    }

    #[test]
    fn test_openai_to_claude_json_schema_appends_instruction() {
        let body = json!({
            "messages": [{"role": "user", "content": "give json"}],
            "response_format": {"type": "json_schema", "json_schema": {"schema": {"type": "object"}}}
        });
        let result = openai_to_claude_request("claude-sonnet-4-5", body, false, None, &Default::default());
        let system_text = result["system"][1]["text"].as_str().unwrap();
        assert!(system_text.contains("strictly follows this JSON schema"));
    }

    #[test]
    fn test_openai_to_claude_tool_choice_shapes() {
        let body = json!({"messages": [], "tool_choice": "required"});
        let r = openai_to_claude_request("m", body, true, None, &Default::default());
        assert_eq!(r["tool_choice"]["type"], "any");

        let body = json!({"messages": [], "tool_choice": {"type": "function", "function": {"name": "f"}}});
        let r = openai_to_claude_request("m", body, true, None, &Default::default());
        assert_eq!(r["tool_choice"], json!({"type": "tool", "name": "f"}));

        let body = json!({"messages": [], "tool_choice": "bogus"});
        let r = openai_to_claude_request("m", body, true, None, &Default::default());
        assert_eq!(r["tool_choice"]["type"], "auto");
    }

    #[test]
    fn test_openai_to_claude_user_image_data_uri() {
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "look"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
        ]}]});
        let result = openai_to_claude_request("m", body, true, None, &Default::default());
        let content = result["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/png");
        assert_eq!(content[1]["source"]["data"], "AAAA");
    }
}
