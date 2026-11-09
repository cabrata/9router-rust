//! OpenAI format helpers. Port of open-sse/translator/formats/openai.js.

use nr_core::Json;
use serde_json::json;

use crate::schema::{claude_blocks, openai_blocks, roles};

/// Content types that survive OpenAI-format normalization.
pub const VALID_OPENAI_CONTENT_TYPES: &[&str] = &[
    openai_blocks::TEXT,
    openai_blocks::IMAGE_URL,
    openai_blocks::INPUT_AUDIO,
    openai_blocks::AUDIO_URL,
    openai_blocks::FILE,
];

/// Message types accepted by OpenAI-compatible endpoints.
pub const VALID_OPENAI_MESSAGE_TYPES: &[&str] = &[roles::USER, roles::ASSISTANT, roles::SYSTEM, roles::TOOL, roles::DEVELOPER];

fn strip_block(block: &Json, keep_cache: bool) -> Json {
    let Some(obj) = block.as_object() else {
        return block.clone();
    };
    let mut out = serde_json::Map::new();
    for (k, v) in obj {
        if k == "signature" {
            continue;
        }
        if k == "cache_control" && !keep_cache {
            continue;
        }
        out.insert(k.clone(), v.clone());
    }
    Json::Object(out)
}

/// Filter messages to OpenAI standard format.
/// Remove: thinking, redacted_thinking, signature, and other non-OpenAI blocks.
/// `opts.preserve_cache_control`: keep cache_control on content blocks (e.g. for DashScope/alicode).
pub fn filter_to_openai_format(body: &mut Json, opts: &FilterOptions) {
    if !body.get("messages").map(|m| m.is_array()).unwrap_or(false) {
        return;
    }
    let keep_cache = opts.preserve_cache_control;

    // Per-message block filtering.
    if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut() {
            // Normalize developer role to system (many providers don't support developer)
            if msg.get("role").and_then(|v| v.as_str()) == Some(roles::DEVELOPER) {
                msg["role"] = json!(roles::SYSTEM);
            }

            // Keep tool messages as-is (OpenAI format)
            if msg.get("role").and_then(|v| v.as_str()) == Some(roles::TOOL) {
                continue;
            }

            // Keep assistant messages with tool_calls as-is
            if msg.get("role").and_then(|v| v.as_str()) == Some(roles::ASSISTANT)
                && msg.get("tool_calls").is_some()
            {
                continue;
            }

            let Some(content) = msg.get("content").and_then(|c| c.as_array()) else {
                continue;
            };

            let mut filtered: Vec<Json> = Vec::new();
            for block in content {
                let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                // Skip thinking blocks
                if block_type == claude_blocks::THINKING || block_type == claude_blocks::REDACTED_THINKING {
                    continue;
                }
                if VALID_OPENAI_CONTENT_TYPES.contains(&block_type) {
                    filtered.push(strip_block(block, keep_cache));
                } else if block_type == claude_blocks::TOOL_USE {
                    // Converted to tool_calls format elsewhere
                    continue;
                } else if block_type == claude_blocks::TOOL_RESULT {
                    // Keep tool_result but clean it
                    filtered.push(strip_block(block, keep_cache));
                }
            }

            // If all content was filtered, add empty text
            if filtered.is_empty() {
                filtered.push(json!({"type": openai_blocks::TEXT, "text": ""}));
            }
            msg["content"] = Json::Array(filtered);
        }
    }

    // Drop messages that are only empty text (but never tool messages).
    if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        messages.retain(|msg| {
            if msg.get("role").and_then(|v| v.as_str()) == Some(roles::TOOL) {
                return true;
            }
            if msg.get("role").and_then(|v| v.as_str()) == Some(roles::ASSISTANT)
                && msg.get("tool_calls").is_some()
            {
                return true;
            }
            match msg.get("content") {
                Some(Json::String(s)) => !s.trim().is_empty(),
                Some(Json::Array(blocks)) => blocks.iter().any(|b| {
                    let is_text = b.get("type").and_then(|v| v.as_str()) == Some(openai_blocks::TEXT);
                    if is_text {
                        b.get("text").and_then(|v| v.as_str()).map(|t| !t.trim().is_empty()).unwrap_or(false)
                    } else {
                        true
                    }
                }),
                _ => true,
            }
        });
    }

    // Remove empty tools array (some providers like QWEN reject it)
    if body.get("tools").and_then(|t| t.as_array()).map(|t| t.is_empty()).unwrap_or(false) {
        if let Some(obj) = body.as_object_mut() {
            obj.remove("tools");
        }
    }

    // Normalize tools to OpenAI format (from Claude, Gemini, etc.)
    if let Some(tools) = body.get_mut("tools").and_then(|t| t.as_array_mut()) {
        if !tools.is_empty() {
            let mut normalized: Vec<Json> = Vec::new();
            for tool in tools.iter() {
                // Already OpenAI format
                if tool.get("type").and_then(|v| v.as_str()) == Some(openai_blocks::FUNCTION)
                    && tool.get("function").is_some()
                {
                    normalized.push(tool.clone());
                    continue;
                }

                // Claude format: {name, description, input_schema}
                if tool.get("name").is_some()
                    && (tool.get("input_schema").is_some() || tool.get("description").is_some())
                {
                    let params = tool
                        .get("input_schema")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                    normalized.push(json!({
                        "type": openai_blocks::FUNCTION,
                        "function": {
                            "name": tool.get("name").cloned().unwrap_or(Json::Null),
                            "description": string_or_empty(tool.get("description")),
                            "parameters": params
                        }
                    }));
                    continue;
                }

                // Gemini format: {functionDeclarations: [{name, description, parameters}]}
                if let Some(decls) = tool.get("functionDeclarations").and_then(|d| d.as_array()) {
                    for f in decls {
                        normalized.push(json!({
                            "type": openai_blocks::FUNCTION,
                            "function": {
                                "name": f.get("name").cloned().unwrap_or(Json::Null),
                                "description": string_or_empty(f.get("description")),
                                "parameters": f.get("parameters").cloned()
                                    .unwrap_or_else(|| json!({"type": "object", "properties": {}}))
                            }
                        }));
                    }
                    continue;
                }

                normalized.push(tool.clone());
            }
            *tools = normalized;
        }
    }

    // Normalize tool_choice to OpenAI format
    if let Some(choice) = body.get("tool_choice").filter(|c| c.is_object()) {
        let normalized = match choice.get("type").and_then(|v| v.as_str()) {
            Some("auto") => Some(json!("auto")),
            Some("any") => Some(json!("required")),
            Some("tool") => choice.get("name").cloned().map(|name| {
                json!({"type": openai_blocks::FUNCTION, "function": {"name": name}})
            }),
            _ => None,
        };
        if let Some(normalized) = normalized {
            body["tool_choice"] = normalized;
        }
    }
}

fn string_or_empty(v: Option<&Json>) -> String {
    match v {
        Some(Json::String(s)) => s.clone(),
        Some(Json::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

#[derive(Clone, Debug, Default)]
pub struct FilterOptions {
    pub preserve_cache_control: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_to_openai_format_noop() {
        let mut body = json!({
            "messages": [{"role": "user", "content": "hello"}]
        });
        let opts = FilterOptions::default();
        filter_to_openai_format(&mut body, &opts);
        assert!(body.is_object());
        assert_eq!(body["messages"][0]["content"], "hello");
    }

    #[test]
    fn test_developer_role_normalized() {
        let mut body = json!({"messages": [{"role": "developer", "content": "hi"}]});
        filter_to_openai_format(&mut body, &FilterOptions::default());
        assert_eq!(body["messages"][0]["role"], "system");
    }

    #[test]
    fn test_thinking_blocks_dropped() {
        let mut body = json!({"messages": [{
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "secret", "signature": "x"},
                {"type": "text", "text": "hello"}
            ]
        }]});
        filter_to_openai_format(&mut body, &FilterOptions::default());
        assert_eq!(body["messages"][0]["content"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["content"][0]["text"], "hello");
    }

    #[test]
    fn test_empty_text_messages_dropped() {
        let mut body = json!({"messages": [
            {"role": "user", "content": [{"type": "thinking", "thinking": "x"}]},
            {"role": "user", "content": "kept"}
        ]});
        filter_to_openai_format(&mut body, &FilterOptions::default());
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        // First message had all content filtered → replaced with empty text, then dropped.
        assert_eq!(msgs[0]["content"], "kept");
    }

    #[test]
    fn test_claude_tools_normalized() {
        let mut body = json!({
            "messages": [],
            "tools": [{"name": "get", "description": "d", "input_schema": {"type": "object"}}]
        });
        filter_to_openai_format(&mut body, &FilterOptions::default());
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "get");
    }

    #[test]
    fn test_gemini_tools_flattened() {
        let mut body = json!({
            "messages": [],
            "tools": [{"functionDeclarations": [
                {"name": "a", "description": "one", "parameters": {"type": "object"}},
                {"name": "b"}
            ]}]
        });
        filter_to_openai_format(&mut body, &FilterOptions::default());
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[1]["function"]["name"], "b");
    }

    #[test]
    fn test_tool_choice_any_to_required() {
        let mut body = json!({"messages": [], "tool_choice": {"type": "any"}});
        filter_to_openai_format(&mut body, &FilterOptions::default());
        assert_eq!(body["tool_choice"], "required");
    }

    #[test]
    fn test_tool_choice_tool_named() {
        let mut body = json!({"messages": [], "tool_choice": {"type": "tool", "name": "f"}});
        filter_to_openai_format(&mut body, &FilterOptions::default());
        assert_eq!(body["tool_choice"]["function"]["name"], "f");
    }

    #[test]
    fn test_cache_control_preserved_when_asked() {
        let mut body = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}, "signature": "s"}
        ]}]});
        filter_to_openai_format(&mut body, &FilterOptions { preserve_cache_control: true });
        let block = &body["messages"][0]["content"][0];
        assert!(block.get("cache_control").is_some());
        assert!(block.get("signature").is_none());
    }

    #[test]
    fn test_empty_tools_removed() {
        let mut body = json!({"messages": [], "tools": []});
        filter_to_openai_format(&mut body, &FilterOptions::default());
        assert!(body.get("tools").is_none());
    }
}
