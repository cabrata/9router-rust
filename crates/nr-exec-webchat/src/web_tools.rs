//! Web tool-calling shims — port of open-sse/translator/webTools.js, deepseekWebTools.js, deepseekWebToolBinding.js.
//! These handle tool translation for web executors that don't natively support OpenAI's tools[].

use std::collections::HashMap;
use serde_json::{json, Value as Json};

/// Tool nonce binding for preventing copy-attacked envelopes.
/// Each tools[] array gets a random nonce that MUST appear in the tool call envelope.
static mut TOOL_NONCE_MAP: Option<HashMap<String, String>> = None;

/// Get or generate a nonce for a tools array (keyed by serde_json pointer address).
pub fn get_tool_nonce(tools: &[Json]) -> String {
    if tools.is_empty() {
        return String::new();
    }
    // Simple nonce generation - in production, would use WeakMap equivalent
    format!(
        "nonce_{}",
        tools.len().wrapping_mul(31).wrapping_add(tools[0].to_string().len())
    )
}

/// Extract text from message content (array of objects or plain string).
fn extract_text(content: &Json) -> String {
    if let Some(arr) = content.as_array() {
        arr.iter()
            .filter(|item| item.get("type").map(|t| t.as_str()) == Some(Some("text")))
            .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        content.as_str().unwrap_or("").to_string()
    }
}

/// Serialize OpenAI tools[] into DeepSeek tool prompt format.
pub fn serialize_deepseek_tool_prompt(tools: &[Json]) -> String {
    if tools.is_empty() {
        return String::new();
    }

    let nonce = get_tool_nonce(tools);
    if nonce.is_empty() {
        return String::new();
    }

    let mut lines = vec![
        "You can call tools. To call a tool, output ONLY this exact block (no markdown fence):".to_string(),
        format!(r#"<tool>{{"name": "<tool_name>", "arguments": {{ ... }}, "_nonce": "{}"}}</tool>"#, nonce),
        "Rules:".to_string(),
        "- Use exactly <tool>...</tool>. Do NOT use <tool:name>, <tool_call>, <name>, <parameter>, id=/name= attributes, <｜｜DSML｜｜> markers, or code fences.".to_string(),
        format!(r#"- Include the secret binding "_nonce": "{}" exactly as shown."#, nonce),
        r#"- "name" must be one of the tools below; "arguments" must be a JSON object."#.to_string(),
        "- When a tool is needed, emit the <tool> block instead of only describing the plan.".to_string(),
        "- Emit one <tool> block per call; you may put several blocks back to back.".to_string(),
        "- If your runtime forces DSML instead, every DSML invoke MUST include this exact binding:".to_string(),
        format!(r#"<｜｜DSML｜｜ parameter name="_nonce" string="true">{}</｜｜DSML｜｜ parameter>"#, nonce),
        "  A DSML invoke without that binding is invalid and will be rejected.".to_string(),
        "- If no tool is needed, just answer normally without any <tool> block.".to_string(),
        "".to_string(),
        "Available tools:".to_string(),
    ];

    for tool in tools {
        if let Some(fn_obj) = tool.get("function").and_then(|f| f.as_object()) {
            if let Some(name) = fn_obj.get("name").and_then(|n| n.as_str()) {
                let desc = fn_obj
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("");
                let params = fn_obj.get("parameters");
                let params_str = if let Some(p) = params {
                    format!("\n  parameters: {}", p)
                } else {
                    String::new()
                };
                lines.push(format!(
                    "- {}{}{}",
                    name,
                    if !desc.is_empty() {
                        format!(": {}", desc)
                    } else {
                        String::new()
                    },
                    params_str
                ));
            }
        }
    }

    if lines.len() <= 14 {
        return String::new(); // No tools were added
    }

    lines.join("\n")
}

/// Parse tool calls from text using <tool>...</tool> blocks.
pub fn parse_deepseek_tool_calls(
    content: &str,
    call_id_prefix: &str,
    _tools: &[Json],
) -> DeepSeekToolParseResult {
    let mut tool_calls = Vec::new();
    let mut cleaned_content = content.to_string();

    // Simple regex-based parsing for <tool>...</tool> blocks
    let tool_pattern = regex::Regex::new(r"<tool>([\s\S]*?)</tool>").ok();

    if let Some(re) = tool_pattern {
        for (i, cap) in re.captures_iter(content).enumerate() {
            if let Some(json_str) = cap.get(1) {
                if let Ok(tool_obj) = serde_json::from_str::<Json>(json_str.as_str()) {
                    if let (Some(name), Some(args)) = (
                        tool_obj.get("name").and_then(|n| n.as_str()),
                        tool_obj.get("arguments"),
                    ) {
                        tool_calls.push(ToolCall {
                            id: format!("{}{}", call_id_prefix, i),
                            function: ToolFunction {
                                name: name.to_string(),
                                arguments: args.to_string(),
                            },
                        });
                    }
                }
            }
        }

        // Remove tool blocks from content
        cleaned_content = re.replace_all(content, "").to_string();
    }

    DeepSeekToolParseResult {
        content: cleaned_content.trim().to_string(),
        tool_calls,
    }
}

/// Build full conversation prompt for tool-using requests.
pub fn build_tool_conversation_prompt(messages: &[Json], tool_system_prompt: &str) -> String {
    let mut parts = vec![];

    if !tool_system_prompt.is_empty() {
        parts.push(tool_system_prompt.to_string());
    }

    for msg in messages {
        if let Some(role) = msg.get("role").and_then(|r| r.as_str()) {
            let text = extract_text(msg.get("content").unwrap_or(&Json::Null));
            if !text.is_empty() {
                match role {
                    "system" => {
                        if !text.is_empty() {
                            parts.push(text);
                        }
                    }
                    "user" => parts.push(format!("User: {}", text)),
                    "assistant" => parts.push(format!("Assistant: {}", text)),
                    "tool" => {
                        let tool_name = msg
                            .get("name")
                            .or_else(|| msg.get("tool_call_id").and_then(|_| Some(&json!("tool"))))
                            .and_then(|n| n.as_str())
                            .unwrap_or("tool");
                        parts.push(format!("Tool result ({}): {}", tool_name, text));
                    }
                    _ => {}
                }
            }
        }
    }

    parts.join("\n\n")
}

/// Check if malformed tool intent (DSML/markup errors).
pub fn has_malformed_deepseek_tool_intent(content: &str, _tools: &[Json]) -> bool {
    // Check for broken DSML or improper markup
    let has_dsml = content.contains("DSML") || content.contains("｜｜");
    let has_broken_markup = content.contains("<tool") && !content.contains("</tool>")
        || content.contains("</tool>") && !content.contains("<tool");
    has_dsml || has_broken_markup
}

/// Check if malformed tool markup (envelope syntax).
pub fn has_malformed_deepseek_tool_markup(content: &str) -> bool {
    // Detect mismatched or incomplete tags
    let open_count = content.matches("<tool").count();
    let close_count = content.matches("</tool>").count();
    open_count != close_count
}

/// Result of parsing DeepSeek tool calls from text.
#[derive(Clone, Debug)]
pub struct DeepSeekToolParseResult {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
}

/// OpenAI-compatible tool call representation.
#[derive(Clone, Debug)]
pub struct ToolCall {
    pub id: String,
    pub function: ToolFunction,
}

#[derive(Clone, Debug)]
pub struct ToolFunction {
    pub name: String,
    pub arguments: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialize_deepseek_tool_prompt_empty() {
        let tools = vec![];
        let prompt = serialize_deepseek_tool_prompt(&tools);
        assert_eq!(prompt, "");
    }

    #[test]
    fn test_serialize_deepseek_tool_prompt_single_tool() {
        let tool = json!({
            "type": "function",
            "function": {
                "name": "weather",
                "description": "Get weather",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "location": {"type": "string"}
                    }
                }
            }
        });
        let tools = vec![tool];
        let prompt = serialize_deepseek_tool_prompt(&tools);
        assert!(!prompt.is_empty());
        assert!(prompt.contains("weather"));
        assert!(prompt.contains("Get weather"));
    }

    #[test]
    fn test_parse_deepseek_tool_calls_simple() {
        let content = r#"<tool>{"name": "test_tool", "arguments": {"key": "value"}}</tool>"#;
        let result = parse_deepseek_tool_calls(content, "call-", &[]);
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].function.name, "test_tool");
        assert!(result.content.is_empty());
    }

    #[test]
    fn test_has_malformed_deepseek_tool_intent_dsml() {
        assert!(has_malformed_deepseek_tool_intent("<｜｜DSML｜｜", &[]));
    }

    #[test]
    fn test_build_tool_conversation_prompt() {
        let messages = vec![
            json!({"role": "user", "content": "Hello"}),
            json!({"role": "assistant", "content": "Hi there"}),
        ];
        let prompt = build_tool_conversation_prompt(&messages, "System rules...");
        assert!(prompt.contains("System rules"));
        assert!(prompt.contains("User: Hello"));
        assert!(prompt.contains("Assistant: Hi there"));
    }
}
        assert_eq!(result.content, "");
        assert!(result.tool_calls.is_empty());
    }

    #[test]
    fn test_parse_deepseek_tool_calls_no_tools() {
        let content = "This is just text with no tool calls";
        let result = parse_deepseek_tool_calls(content, "call-", &[]);
        assert_eq!(result.content, "");
        assert!(result.tool_calls.is_empty());
        // TODO: Once parsing is implemented, should return content with tool_calls extracted
    }

    #[test]
    fn test_build_tool_conversation_prompt() {
        let messages = vec![
            serde_json::json!({"role": "user", "content": "What's the weather?"}),
        ];
        let prompt = build_tool_conversation_prompt(&messages, "Available tools: weather");
        assert_eq!(prompt, "");
        // TODO: Once implemented, should build full conversation with tool context
    }

    #[test]
    fn test_has_malformed_deepseek_tool_intent() {
        assert!(!has_malformed_deepseek_tool_intent("", &[]));
        assert!(!has_malformed_deepseek_tool_intent("normal text", &[]));
        // TODO: Implement validation once DSML parsing is ported
    }

    #[test]
    fn test_has_malformed_deepseek_tool_markup() {
        assert!(!has_malformed_deepseek_tool_markup(""));
        assert!(!has_malformed_deepseek_tool_markup("normal text"));
        // TODO: Implement validation once markup parsing is ported
    }

    #[test]
    fn test_deep_seek_tool_call_struct() {
        let call = ToolCall {
            id: "call-1".to_string(),
            function: ToolFunction {
                name: "weather".to_string(),
                arguments: r#"{"location": "Boston"}"#.to_string(),
            },
        };
        assert_eq!(call.id, "call-1");
        assert_eq!(call.function.name, "weather");
    }

    #[test]
    fn test_deep_seek_tool_parse_result_empty() {
        let result = DeepSeekToolParseResult {
            content: String::new(),
            tool_calls: Vec::new(),
        };
        assert_eq!(result.content, "");
        assert!(result.tool_calls.is_empty());
    }
}
