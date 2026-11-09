//! OpenAI to Cursor request translator. Port of open-sse/translator/request/openai-to-cursor.js.

use std::collections::HashMap;

use nr_core::{Credentials, Json};
use serde_json::json;

use crate::schema::{claude_blocks, openai_blocks, roles};
use crate::TranslationOptions;

const DEFAULT_MIN_TOKENS: i64 = 32000;

fn extract_content(content: &Json) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(arr) = content.as_array() {
        return arr
            .iter()
            .filter_map(|p| {
                if p.get("type").and_then(|v| v.as_str()) == Some(openai_blocks::TEXT) {
                    p.get("text").and_then(|v| v.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("");
    }
    String::new()
}

fn sanitize_tool_result_text(text: &str) -> String {
    text.chars()
        .filter(|c| {
            let u = *c as u32;
            !(u <= 0x08 || (0x0B..=0x0C).contains(&u) || (0x0E..=0x1F).contains(&u) || u == 0x7F)
        })
        .collect()
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn build_tool_result_block(tool_name: &str, tool_call_id: &str, result_text: &str) -> String {
    let clean = sanitize_tool_result_text(result_text);
    format!(
        "<tool_result>\n<tool_name>{}</tool_name>\n<tool_call_id>{}</tool_call_id>\n<result>{}</result>\n</tool_result>",
        escape_xml(if tool_name.is_empty() { "tool" } else { tool_name }),
        escape_xml(tool_call_id),
        escape_xml(&clean)
    )
}

fn normalize_tool_call_id(id: &str) -> &str {
    id.split('\n').next().unwrap_or("")
}

fn convert_messages(messages: &[Json]) -> Vec<Json> {
    let mut result = Vec::new();
    let mut tool_meta_map: HashMap<String, String> = HashMap::new();

    // Map tool_call_id -> tool name
    for msg in messages {
        if msg.get("role").and_then(|v| v.as_str()) == Some(roles::ASSISTANT) {
            if let Some(tool_calls) = msg.get("tool_calls").and_then(|tc| tc.as_array()) {
                for tc in tool_calls {
                    let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    let name = tc.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()).unwrap_or("tool");
                    if !id.is_empty() {
                        tool_meta_map.insert(id.to_string(), name.to_string());
                        let norm = normalize_tool_call_id(id);
                        if norm != id {
                            tool_meta_map.insert(norm.to_string(), name.to_string());
                        }
                    }
                }
            }
            if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
                for part in content {
                    if part.get("type").and_then(|v| v.as_str()) == Some(claude_blocks::TOOL_USE) {
                        let id = part.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        let name = part.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                        if !id.is_empty() {
                            tool_meta_map.insert(id.to_string(), name.to_string());
                        }
                    }
                }
            }
        }
    }

    for msg in messages {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");

        if role == roles::SYSTEM {
            result.push(json!({
                "role": roles::USER,
                "content": format!("[System Instructions]\n{}", extract_content(&msg.get("content").cloned().unwrap_or(Json::Null)))
            }));
            continue;
        }

        if role == roles::TOOL {
            let tool_content = extract_content(&msg.get("content").cloned().unwrap_or(Json::Null));
            let tool_call_id = msg.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("");
            let tool_name = msg
                .get("name")
                .and_then(|v| v.as_str())
                .or_else(|| tool_meta_map.get(tool_call_id).map(|s| s.as_str()))
                .unwrap_or("tool");
            result.push(json!({
                "role": roles::USER,
                "content": build_tool_result_block(tool_name, tool_call_id, &tool_content)
            }));
            continue;
        }

        if role == roles::USER || role == roles::ASSISTANT {
            if role == roles::USER && msg.get("content").map_or(false, |c| c.is_array()) {
                let mut parts = Vec::new();
                for block in msg["content"].as_array().unwrap() {
                    let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if block_type == claude_blocks::TEXT {
                        if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                            parts.push(text.to_string());
                        }
                    } else if block_type == claude_blocks::TOOL_RESULT {
                        let tool_call_id = block.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
                        let tool_name = tool_meta_map
                            .get(tool_call_id)
                            .or_else(|| tool_meta_map.get(normalize_tool_call_id(tool_call_id)))
                            .map(|s| s.as_str())
                            .unwrap_or("tool");
                        let tool_content = extract_content(&block.get("content").cloned().unwrap_or(Json::Null));
                        parts.push(build_tool_result_block(tool_name, tool_call_id, &tool_content));
                    }
                }
                let joined = parts.into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n");
                if !joined.is_empty() {
                    result.push(json!({"role": roles::USER, "content": joined}));
                }
                continue;
            }

            let content = extract_content(&msg.get("content").cloned().unwrap_or(Json::Null));

            if role == roles::ASSISTANT && msg.get("tool_calls").and_then(|t| t.as_array()).map_or(false, |t| !t.is_empty()) {
                let mut assistant_msg = json!({"role": roles::ASSISTANT, "content": content});
                let tool_calls: Vec<Json> = msg["tool_calls"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|tc| {
                        let mut tc = tc.clone();
                        if let Some(obj) = tc.as_object_mut() {
                            obj.remove("index");
                        }
                        tc
                    })
                    .collect();
                assistant_msg["tool_calls"] = Json::Array(tool_calls);
                result.push(assistant_msg);
            } else if role == roles::ASSISTANT && msg.get("content").map_or(false, |c| c.is_array()) {
                let extracted: Vec<Json> = msg["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some(claude_blocks::TOOL_USE))
                    .filter_map(|b| {
                        let id = b.get("id").and_then(|v| v.as_str())?;
                        if id.is_empty() {
                            return None;
                        }
                        let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                        let input = b.get("input").cloned().unwrap_or_else(|| json!({}));
                        Some(json!({
                            "id": id,
                            "type": openai_blocks::FUNCTION,
                            "function": {
                                "name": name,
                                "arguments": serde_json::to_string(&input).unwrap_or_default()
                            }
                        }))
                    })
                    .collect();
                if !extracted.is_empty() {
                    result.push(json!({
                        "role": roles::ASSISTANT,
                        "content": content,
                        "tool_calls": extracted
                    }));
                } else if !content.is_empty() {
                    result.push(json!({"role": roles::ASSISTANT, "content": content}));
                }
            } else if !content.is_empty() {
                result.push(json!({"role": role, "content": content}));
            }
        }
    }

    result
}

/// Convert OpenAI request to Cursor format.
pub fn openai_to_cursor_request(
    _model: &str,
    mut body: Json,
    _stream: bool,
    _credentials: Option<&mut Credentials>,
    _options: &TranslationOptions,
) -> Json {
    let messages = body
        .get("messages")
        .and_then(|m| m.as_array())
        .map(|m| convert_messages(m))
        .unwrap_or_default();

    if let Some(obj) = body.as_object_mut() {
        for key in ["user", "metadata", "tool_choice", "stream_options", "system"] {
            obj.remove(key);
        }
    }
    body["messages"] = Json::Array(messages);
    body["max_tokens"] = json!(DEFAULT_MIN_TOKENS);
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_openai_to_cursor_system_converted_to_user() {
        let body = json!({
            "messages": [
                {"role": "system", "content": "You are helpful"},
                {"role": "user", "content": "hello"}
            ]
        });
        let result = openai_to_cursor_request("cursor-fast", body, false, None, &Default::default());
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "user");
        assert!(messages[0]["content"].as_str().unwrap().contains("[System Instructions]"));
        assert_eq!(result["max_tokens"], DEFAULT_MIN_TOKENS);
    }

    #[test]
    fn test_openai_to_cursor_tool_message_xml() {
        let body = json!({
            "messages": [
                {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "c1",
                        "function": {"name": "lookup", "arguments": "{}"}
                    }]
                },
                {"role": "tool", "tool_call_id": "c1", "content": "result data"}
            ]
        });
        let result = openai_to_cursor_request("cursor-fast", body, false, None, &Default::default());
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages[1]["role"], "user");
        let content = messages[1]["content"].as_str().unwrap();
        assert!(content.contains("<tool_result>"));
        assert!(content.contains("<tool_name>lookup</tool_name>"));
        assert!(content.contains("<result>result data</result>"));
    }
}
