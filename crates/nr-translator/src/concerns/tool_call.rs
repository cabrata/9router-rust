//! Tool call helper functions. Port of open-sse/translator/concerns/toolCall.js.

use regex::Regex;
use once_cell::sync::Lazy;
use nr_core::Json;
use crate::schema::claude_blocks;

static TOOL_ID_PATTERN: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[a-zA-Z0-9_-]+$").unwrap());

/// Generate deterministic tool call ID from position + tool name (cache-friendly).
pub fn generate_tool_call_id(msg_index: usize, tc_index: usize, tool_name: &str) -> String {
    let name_part = if tool_name.is_empty() {
        String::new()
    } else {
        format!(
            "_{}",
            tool_name.chars().filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-').collect::<String>()
        )
    };
    format!("call_msg{}_tc{}{}", msg_index, tc_index, name_part)
}

/// Sanitize ID to match Anthropic pattern: keep only alphanumeric, underscore, hyphen.
fn sanitize_tool_id(id: &str) -> Option<String> {
    if id.is_empty() {
        return None;
    }
    let sanitized: String = id.chars().filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-').collect();
    if sanitized.is_empty() {
        None
    } else {
        Some(sanitized)
    }
}

/// Ensure all tool_calls have valid id field and arguments is string (some providers require it).
pub fn ensure_tool_call_ids(body: &mut Json) {
    let messages = match body.get_mut("messages").and_then(|v| v.as_array_mut()) {
        Some(msgs) => msgs,
        None => return,
    };

    for (i, msg) in messages.iter_mut().enumerate() {
        let msg_obj = match msg.as_object_mut() {
            Some(m) => m,
            None => continue,
        };

        let role = msg_obj.get("role").and_then(|v| v.as_str()).unwrap_or("").to_string();

        // Handle assistant message with tool_calls
        if role == "assistant" {
            if let Some(tool_calls_arr) = msg_obj.get_mut("tool_calls") {
                if let Some(tool_calls) = tool_calls_arr.as_array_mut() {
                    for (j, tc) in tool_calls.iter_mut().enumerate() {
                        let tc_obj = match tc.as_object_mut() {
                            Some(t) => t,
                            None => continue,
                        };

                        // Validate or regenerate ID
                        let (current_id, tool_name) = {
                            let cid = tc_obj.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            let tn = tc_obj.get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string();
                            (cid, tn)
                        };

                        if current_id.is_empty() || !TOOL_ID_PATTERN.is_match(&current_id) {
                            let sanitized = sanitize_tool_id(&current_id);
                            let new_id = sanitized.unwrap_or_else(|| generate_tool_call_id(i, j, &tool_name));
                            tc_obj.insert("id".to_string(), Json::String(new_id));
                        }

                        // Ensure type is set
                        if !tc_obj.contains_key("type") {
                            tc_obj.insert("type".to_string(), Json::String("function".to_string()));
                        }

                        // Ensure arguments is JSON string, not object
                        if let Some(function) = tc_obj.get_mut("function").and_then(|f| f.as_object_mut()) {
                            if let Some(Json::Object(_)) = function.get("arguments") {
                                let args_obj = function.get("arguments").cloned().unwrap_or(Json::Null);
                                function.insert("arguments".to_string(), Json::String(args_obj.to_string()));
                            }
                        }
                    }
                }
            }
        }

        // Validate tool_call_id in tool messages (role: "tool")
        if role == "tool" {
            if let Some(tool_call_id) = msg_obj.get("tool_call_id").and_then(|v| v.as_str()) {
                if !TOOL_ID_PATTERN.is_match(tool_call_id) {
                    let sanitized = sanitize_tool_id(tool_call_id);
                    let new_id = sanitized.unwrap_or_else(|| generate_tool_call_id(i, 0, ""));
                    msg_obj.insert("tool_call_id".to_string(), Json::String(new_id));
                }
            }
        }

        // Validate tool_use blocks in content (Claude format)
        if let Some(content_arr) = msg_obj.get_mut("content") {
            if let Some(content) = content_arr.as_array_mut() {
                for (k, block) in content.iter_mut().enumerate() {
                    let block_obj = match block.as_object_mut() {
                        Some(b) => b,
                        None => continue,
                    };

                    let (_block_type, id_field, id_value, name_field) = {
                        let bt = block_obj.get("type").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let (idf, iv) = if bt == "tool_use" {
                            ("id", block_obj.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string())
                        } else if bt == claude_blocks::TOOL_RESULT {
                            ("tool_use_id", block_obj.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("").to_string())
                        } else {
                            ("", "".to_string())
                        };
                        let nf = block_obj.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                        (bt, idf, iv, nf)
                    };

                    if id_field == "id" && !id_value.is_empty() {
                        if !TOOL_ID_PATTERN.is_match(&id_value) {
                            let sanitized = sanitize_tool_id(&id_value);
                            let new_id = sanitized.unwrap_or_else(|| generate_tool_call_id(i, k, &name_field));
                            block_obj.insert("id".to_string(), Json::String(new_id));
                        }
                    } else if id_field == "tool_use_id" && !id_value.is_empty() {
                        if !TOOL_ID_PATTERN.is_match(&id_value) {
                            let sanitized = sanitize_tool_id(&id_value);
                            let new_id = sanitized.unwrap_or_else(|| generate_tool_call_id(i, k, ""));
                            block_obj.insert("tool_use_id".to_string(), Json::String(new_id));
                        }
                    }
                }
            }
        }
    }
}

/// Get tool_call ids from assistant message.
pub fn get_tool_call_ids(msg: &Json) -> Vec<String> {
    let mut ids = Vec::new();

    let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
    if role != "assistant" {
        return ids;
    }

    // OpenAI format: tool_calls array
    if let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
        for tc in tool_calls {
            if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                ids.push(id.to_string());
            }
        }
    }

    // Claude format: tool_use blocks in content
    if let Some(content) = msg.get("content").and_then(|v| v.as_array()) {
        for block in content {
            if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                if let Some(id) = block.get("id").and_then(|v| v.as_str()) {
                    ids.push(id.to_string());
                }
            }
        }
    }

    ids
}

/// Check if message has tool_result for given ids.
pub fn has_tool_results(msg: &Json, tool_call_ids: &[String]) -> bool {
    if tool_call_ids.is_empty() {
        return false;
    }

    let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");

    // OpenAI format: role = "tool" with tool_call_id
    if role == "tool" {
        if let Some(tool_call_id) = msg.get("tool_call_id").and_then(|v| v.as_str()) {
            return tool_call_ids.contains(&tool_call_id.to_string());
        }
    }

    // Claude format: tool_result blocks in user message content
    if role == "user" {
        if let Some(content) = msg.get("content").and_then(|v| v.as_array()) {
            for block in content {
                if block.get("type").and_then(|v| v.as_str()) == Some(claude_blocks::TOOL_RESULT) {
                    if let Some(tool_use_id) = block.get("tool_use_id").and_then(|v| v.as_str()) {
                        if tool_call_ids.contains(&tool_use_id.to_string()) {
                            return true;
                        }
                    }
                }
            }
        }
    }

    false
}

/// Fix missing tool responses - insert empty tool_result if assistant has tool_use but next message has no tool_result.
pub fn fix_missing_tool_responses(body: &mut Json) {
    let messages = match body.get_mut("messages").and_then(|v| v.as_array_mut()) {
        Some(msgs) => msgs.clone(),
        None => return,
    };

    let mut new_messages = Vec::new();

    for i in 0..messages.len() {
        let msg = messages[i].clone();
        new_messages.push(msg.clone());

        // Get tool call IDs from current message
        let tool_call_ids = get_tool_call_ids(&msg);
        if tool_call_ids.is_empty() {
            continue;
        }

        // Check if next message has tool_result
        let next_msg = if i + 1 < messages.len() { Some(&messages[i + 1]) } else { None };

        if let Some(next) = next_msg {
            if !has_tool_results(next, &tool_call_ids) {
                // Insert tool responses for each tool_call
                for id in tool_call_ids {
                    new_messages.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": id,
                        "content": ""
                    }));
                }
            }
        } else {
            // Last message is assistant with tool_calls - add empty responses
            for id in tool_call_ids {
                new_messages.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": ""
                }));
            }
        }
    }

    body["messages"] = Json::Array(new_messages);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_tool_call_id() {
        let id = generate_tool_call_id(0, 0, "search");
        assert!(id.starts_with("call_msg"));
        assert!(id.contains("_search"));
    }

    #[test]
    fn test_ensure_tool_call_ids() {
        let mut body = serde_json::json!({
            "messages": [
                {
                    "role": "assistant",
                    "tool_calls": [
                        {"function": {"name": "search", "arguments": "{}"}}
                    ]
                }
            ]
        });

        ensure_tool_call_ids(&mut body);

        let tool_call = &body["messages"][0]["tool_calls"][0];
        assert!(tool_call.get("id").is_some());
        assert_eq!(tool_call.get("type"), Some(&Json::String("function".to_string())));
    }
}
