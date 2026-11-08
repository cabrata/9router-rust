use serde_json::{json, Value};
use sha2::{Sha256, Digest};
use std::collections::{HashMap, HashSet};

const CC_ENTRYPOINT: &str = "sdk-cli";
const CLAUDE_TOOL_SUFFIX: &str = "_ide";

/// Generate the billing header expected from Claude Code clients
fn generate_billing_header(payload: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(payload.as_bytes());
    let cch = format!("{:x}", hasher.finalize())[..5].to_string();
    
    let random_bytes = rand::random::<[u8; 2]>();
    let build_hash = format!("{:02x}{:02x}", random_bytes[0], random_bytes[1])[..3].to_string();
    
    let claude_cli_version = "2.1.280"; // From shared.js
    
    format!("x-anthropic-billing-header: cc_version={}.{}; cc_entrypoint={}; cch={};", 
        claude_cli_version, build_hash, CC_ENTRYPOINT, cch)
}

/// Extract Claude session ID from user ID
pub fn extract_claude_session_id_from_user_id(user_id: &str) -> Option<String> {
    if user_id.is_empty() {
        return None;
    }

    if user_id.starts_with('{') {
        if let Ok(obj) = serde_json::from_str::<serde_json::Value>(user_id) {
            if let Some(sid) = obj.get("session_id").and_then(|v| v.as_str()) {
                let clean = sid.trim_start_matches("claude:").trim();
                return if clean.is_empty() { None } else { Some(clean.to_string()) };
            }
        }
        return None;
    }

    let clean = user_id.trim_start_matches("claude:").trim();
    if clean.is_empty() { None } else { Some(clean.to_string()) }
}

/// Cloak tools before sending to Claude provider (anti-ban)
pub fn cloak_claude_tools(body: &serde_json::Value) -> (serde_json::Value, Option<HashMap<String, String>>) {
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        if tools.is_empty() {
            return (body.clone(), None);
        }

        let mut tool_name_map = HashMap::new();
        let mut client_tool_names = HashSet::new();
        let mut client_declarations = Vec::new();

        for tool in tools {
            if let Some(name) = tool.get("name").and_then(|v| v.as_str()) {
                if tool.get("type").is_some() {
                    // Built-in server tool—keep as-is
                    client_declarations.push(tool.clone());
                } else {
                    // Client tool—rename with suffix
                    let suffixed = format!("{}{}", name, CLAUDE_TOOL_SUFFIX);
                    tool_name_map.insert(suffixed.clone(), name.to_string());
                    client_tool_names.insert(name.to_string());
                    
                    let mut new_tool = tool.clone();
                    if let Some(obj) = new_tool.as_object_mut() {
                        obj.insert("name".to_string(), Value::String(suffixed));
                    }
                    client_declarations.push(new_tool);
                }
            }
        }

        // Get CC decoy tools from data
        let decoy_tools = get_cc_decoy_tools();
        let mut all_tools = client_declarations;
        all_tools.extend(decoy_tools);

        // Rename tool_use in messages
        let renamed_messages = if let Some(messages) = body.get("messages").and_then(|v| v.as_array()) {
            let new_msgs: Vec<Value> = messages.iter().map(|msg| {
                if let Some(content) = msg.get("content").and_then(|v| v.as_array()) {
                    let renamed_content: Vec<Value> = content.iter().map(|block| {
                        if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                            if let Some(tool_name) = block.get("name").and_then(|v| v.as_str()) {
                                let mut new_block = block.clone();
                                if let Some(obj) = new_block.as_object_mut() {
                                    obj.insert("name".to_string(), Value::String(format!("{}{}", tool_name, CLAUDE_TOOL_SUFFIX)));
                                }
                                return new_block;
                            }
                        }
                        block.clone()
                    }).collect();

                    let mut new_msg = msg.clone();
                    if let Some(obj) = new_msg.as_object_mut() {
                        obj.insert("content".to_string(), Value::Array(renamed_content));
                    }
                    new_msg
                } else {
                    msg.clone()
                }
            }).collect();
            Some(new_msgs)
        } else {
            None
        };

        let mut cloaked_body = body.clone();
        if let Some(obj) = cloaked_body.as_object_mut() {
            obj.insert("tools".to_string(), Value::Array(all_tools));
            if let Some(new_messages) = renamed_messages {
                obj.insert("messages".to_string(), Value::Array(new_messages));
            }

            // Rewrite tool_choice if it targets a client tool
            if let Some(tool_choice) = obj.get("tool_choice") {
                if let Some(tc_obj) = tool_choice.as_object() {
                    if let (Some("tool"), Some(name)) = (
                        tc_obj.get("type").and_then(|v| v.as_str()),
                        tc_obj.get("name").and_then(|v| v.as_str())
                    ) {
                        if client_tool_names.contains(name) {
                            let mut new_tc = tc_obj.clone();
                            new_tc.insert("name".to_string(), Value::String(format!("{}{}", name, CLAUDE_TOOL_SUFFIX)));
                            obj.insert("tool_choice".to_string(), Value::Object(new_tc));
                        }
                    }
                }
            }
        }

        (cloaked_body, if tool_name_map.is_empty() { None } else { Some(tool_name_map) })
    } else {
        (body.clone(), None)
    }
}

/// Decloak tool names in non-streaming response
pub fn decloak_tool_names(body: &serde_json::Value, tool_name_map: Option<&HashMap<String, String>>) -> serde_json::Value {
    if let Some(content) = body.get("content").and_then(|v| v.as_array()) {
        let new_content: Vec<Value> = content.iter().map(|block| {
            if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                if let Some(name) = block.get("name").and_then(|v| v.as_str()) {
                    if let Some(map) = tool_name_map {
                        if let Some(original) = map.get(name) {
                            let mut new_block = block.clone();
                            if let Some(obj) = new_block.as_object_mut() {
                                obj.insert("name".to_string(), Value::String(original.clone()));
                            }
                            return new_block;
                        }
                    }
                    // Fallback: strip suffix
                    if let Some(original) = strip_cloak_suffix(name) {
                        let mut new_block = block.clone();
                        if let Some(obj) = new_block.as_object_mut() {
                            obj.insert("name".to_string(), Value::String(original));
                        }
                        return new_block;
                    }
                }
            }
            block.clone()
        }).collect();

        let mut new_body = body.clone();
        if let Some(obj) = new_body.as_object_mut() {
            obj.insert("content".to_string(), Value::Array(new_content));
        }
        new_body
    } else {
        body.clone()
    }
}

/// Decloak tool name in streaming SSE chunk
pub fn decloak_stream_chunk(chunk: &serde_json::Value, tool_name_map: Option<&HashMap<String, String>>) -> serde_json::Value {
    if chunk.get("type").and_then(|v| v.as_str()) != Some("content_block_start") {
        return chunk.clone();
    }

    if let Some(block) = chunk.get("content_block").and_then(|v| v.as_object()) {
        if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
            if let Some(name) = block.get("name").and_then(|v| v.as_str()) {
                let original = if let Some(map) = tool_name_map {
                    map.get(name).cloned()
                } else {
                    None
                }.or_else(|| strip_cloak_suffix(name));

                if let Some(orig) = original {
                    let mut new_chunk = chunk.clone();
                    if let Some(chunk_obj) = new_chunk.as_object_mut() {
                        let mut new_block = block.clone();
                        new_block.insert("name".to_string(), Value::String(orig));
                        chunk_obj.insert("content_block".to_string(), Value::Object(new_block));
                    }
                    return new_chunk;
                }
            }
        }
    }

    chunk.clone()
}

/// Apply Claude cloaking to request body
pub fn apply_cloaking(body: &serde_json::Value, api_key: &str, session_id: Option<&str>) -> serde_json::Value {
    if !api_key.contains("sk-ant-oat") {
        return body.clone();
    }

    let mut result = body.clone();

    if let Some(obj) = result.as_object_mut() {
        // Inject billing header (generate before other changes)
        let billing_text = generate_billing_header(&body.to_string());
        let billing_block = json!({"type": "text", "text": billing_text});

        let mut new_system = if let Some(sys) = obj.get("system") {
            if let Some(arr) = sys.as_array() {
                arr.clone()
            } else if let Some(s) = sys.as_str() {
                vec![json!({"type": "text", "text": s})]
            } else {
                vec![]
            }
        } else {
            vec![]
        };

        // Only inject if not already there
        if new_system.first().and_then(|v| v.get("text")).and_then(|v| v.as_str()).map(|t| t.starts_with("x-anthropic-billing-header:")).unwrap_or(false) {
            // Already injected
        } else {
            new_system.insert(0, billing_block);
        }

        obj.insert("system".to_string(), Value::Array(new_system));

        // Inject fake user ID
        let existing_user_id = obj.get("metadata").and_then(|m| m.get("user_id"));
        if existing_user_id.is_none() {
            let fake_id = generate_fake_user_id(session_id, api_key);
            let mut metadata = obj.get("metadata").cloned().unwrap_or_else(|| json!({}));
            if let Some(m) = metadata.as_object_mut() {
                m.insert("user_id".to_string(), Value::String(fake_id));
            }
            obj.insert("metadata".to_string(), metadata);
        }
    }

    result
}

/// Generate fake user ID in Claude Code JSON format
fn generate_fake_user_id(session_id: Option<&str>, api_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("device:{}", api_key).as_bytes());
    let device_id = format!("{:x}", hasher.finalize());

    let mut hasher = Sha256::new();
    hasher.update(format!("account:{}", api_key).as_bytes());
    let hash = format!("{:x}", hasher.finalize());
    let account_uuid = format!(
        "{}-{}-4{}-{:x}{}-{}",
        &hash[..8],
        &hash[8..12],
        &hash[13..16],
        (u32::from_str_radix(&hash[16..17], 16).unwrap_or(0) & 0x3) | 0x8,
        &hash[17..20],
        &hash[20..32]
    );

    let session_uuid = if let Some(sid) = session_id {
        sid.trim_start_matches("claude:").trim().to_string()
    } else {
        uuid::Uuid::new_v4().to_string()
    };

    json!({"device_id": device_id, "account_uuid": account_uuid, "session_id": session_uuid}).to_string()
}

/// Strip trailing CLAUDE_TOOL_SUFFIX
fn strip_cloak_suffix(name: &str) -> Option<String> {
    if name.ends_with(CLAUDE_TOOL_SUFFIX) && !is_cc_decoy_tool_name(name) {
        let original = &name[..name.len() - CLAUDE_TOOL_SUFFIX.len()];
        if !original.is_empty() {
            return Some(original.to_string());
        }
    }
    None
}

/// Check if name is a CC decoy tool
fn is_cc_decoy_tool_name(name: &str) -> bool {
    matches!(name, "Task" | "TaskOutput" | "TaskStop" | "TaskCreate" | "TaskGet" | "TaskUpdate" | "TaskList" | "Bash" | "Glob" | "Grep" | "Read" | "Edit" | "Write" | "NotebookEdit" | "WebFetch" | "WebSearch" | "AskUserQuestion" | "Skill" | "EnterPlanMode" | "ExitPlanMode")
}

/// Get CC decoy tools
fn get_cc_decoy_tools() -> Vec<Value> {
    vec![
        json!({"name": "Task", "description": "This tool is currently unavailable.", "input_schema": {"type": "object", "properties": {}}}),
        json!({"name": "TaskOutput", "description": "This tool is currently unavailable.", "input_schema": {"type": "object", "properties": {}}}),
        json!({"name": "TaskStop", "description": "This tool is currently unavailable.", "input_schema": {"type": "object", "properties": {}}}),
        json!({"name": "TaskCreate", "description": "This tool is currently unavailable.", "input_schema": {"type": "object", "properties": {}}}),
        json!({"name": "TaskGet", "description": "This tool is currently unavailable.", "input_schema": {"type": "object", "properties": {}}}),
        json!({"name": "TaskUpdate", "description": "This tool is currently unavailable.", "input_schema": {"type": "object", "properties": {}}}),
        json!({"name": "TaskList", "description": "This tool is currently unavailable.", "input_schema": {"type": "object", "properties": {}}}),
        json!({"name": "Bash", "description": "This tool is currently unavailable.", "input_schema": {"type": "object", "properties": {}}}),
        json!({"name": "Glob", "description": "This tool is currently unavailable.", "input_schema": {"type": "object", "properties": {}}}),
        json!({"name": "Grep", "description": "This tool is currently unavailable.", "input_schema": {"type": "object", "properties": {}}}),
        json!({"name": "Read", "description": "This tool is currently unavailable.", "input_schema": {"type": "object", "properties": {}}}),
        json!({"name": "Edit", "description": "This tool is currently unavailable.", "input_schema": {"type": "object", "properties": {}}}),
        json!({"name": "Write", "description": "This tool is currently unavailable.", "input_schema": {"type": "object", "properties": {}}}),
        json!({"name": "NotebookEdit", "description": "This tool is currently unavailable.", "input_schema": {"type": "object", "properties": {}}}),
        json!({"name": "WebFetch", "description": "This tool is currently unavailable.", "input_schema": {"type": "object", "properties": {}}}),
        json!({"name": "WebSearch", "description": "This tool is currently unavailable.", "input_schema": {"type": "object", "properties": {}}}),
        json!({"name": "AskUserQuestion", "description": "This tool is currently unavailable.", "input_schema": {"type": "object", "properties": {}}}),
        json!({"name": "Skill", "description": "This tool is currently unavailable.", "input_schema": {"type": "object", "properties": {}}}),
        json!({"name": "EnterPlanMode", "description": "This tool is currently unavailable.", "input_schema": {"type": "object", "properties": {}}}),
        json!({"name": "ExitPlanMode", "description": "This tool is currently unavailable.", "input_schema": {"type": "object", "properties": {}}}),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_claude_session_from_json() {
        let json_id = r#"{"device_id":"abc","account_uuid":"123","session_id":"sess-123"}"#;
        let result = extract_claude_session_id_from_user_id(json_id);
        assert_eq!(result, Some("sess-123".to_string()));
    }

    #[test]
    fn test_strip_cloak_suffix() {
        assert_eq!(strip_cloak_suffix("my_tool_ide"), Some("my_tool".to_string()));
        assert_eq!(strip_cloak_suffix("regular_tool"), None);
        assert_eq!(strip_cloak_suffix("Bash"), None); // Decoy tool
    }
}
