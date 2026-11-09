//! Antigravity to OpenAI request translator. Port of open-sse/translator/request/antigravity-to-openai.js.

use nr_core::{Credentials, Json};
use serde_json::json;

use crate::concerns::{image, message, thinking};
use crate::schema::{openai_blocks, roles};
use crate::TranslationOptions;

/// Recursively convert Antigravity schema types (OBJECT, STRING, etc.) to lowercase
/// and strip unsupported fields like enumDescriptions.
fn normalize_schema_types(schema: &Json) -> Json {
    if !schema.is_object() && !schema.is_array() {
        return schema.clone();
    }
    match schema {
        Json::Array(arr) => Json::Array(arr.iter().map(normalize_schema_types).collect()),
        Json::Object(obj) => {
            let mut result = obj.clone();
            if let Some(t) = result.get("type").and_then(|v| v.as_str()).map(|s| s.to_lowercase()) {
                result.insert("type".to_string(), json!(t));
            }
            result.remove("enumDescriptions");
            if let Some(props) = result.get("properties").and_then(|p| p.as_object()) {
                let normalized: serde_json::Map<String, Json> = props
                    .iter()
                    .map(|(k, v)| (k.clone(), normalize_schema_types(v)))
                    .collect();
                result.insert("properties".to_string(), Json::Object(normalized));
            }
            if let Some(items) = result.get("items") {
                result.insert("items".to_string(), normalize_schema_types(items));
            }
            Json::Object(result)
        }
        _ => schema.clone(),
    }
}

/// Extract text from systemInstruction (string or parts array).
fn extract_text(instruction: &Json) -> String {
    if let Some(s) = instruction.as_str() {
        return s.to_string();
    }
    if let Some(parts) = instruction.get("parts").and_then(|p| p.as_array()) {
        return parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("");
    }
    String::new()
}

fn collapse(parts: &[Json]) -> Json {
    message::collapse_text_parts(parts, &message::CollapsureOptions::default())
}

/// Convert Antigravity content (Gemini-shaped contents entry) to OpenAI message.
fn convert_content(content: &Json) -> Option<Json> {
    let role = match content.get("role").and_then(|v| v.as_str()) {
        Some("model") => roles::ASSISTANT.to_string(),
        Some("user") => roles::USER.to_string(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    let parts = content.get("parts").and_then(|p| p.as_array())?;

    let mut text_parts: Vec<Json> = Vec::new();
    let mut tool_calls: Vec<Json> = Vec::new();
    let mut tool_results: Vec<Json> = Vec::new();
    let mut reasoning_content = String::new();

    for part in parts {
        // Thinking content (thought: true)
        if part.get("thought").and_then(|v| v.as_bool()) == Some(true) {
            if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                reasoning_content.push_str(text);
            }
            continue;
        }

        // Text with thoughtSignature = regular text after thinking
        if part.get("thoughtSignature").is_some() {
            if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                if !text.is_empty() {
                    text_parts.push(json!({"type": openai_blocks::TEXT, "text": text}));
                }
            }
            continue;
        }

        // Regular text
        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
            if !text.is_empty() {
                text_parts.push(json!({"type": openai_blocks::TEXT, "text": text}));
            }
        }

        // Inline data (images)
        if let Some(inline) = part.get("inlineData").filter(|v| v.is_object()) {
            let mime = inline.get("mimeType").and_then(|v| v.as_str()).unwrap_or("image/png");
            let data = inline.get("data").and_then(|v| v.as_str()).unwrap_or("");
            text_parts.push(json!({
                "type": openai_blocks::IMAGE_URL,
                "image_url": {"url": image::encode_data_uri(mime, data)}
            }));
        }

        // Function call
        if let Some(fc) = part.get("functionCall").filter(|v| v.is_object()) {
            let id = fc.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let name = fc.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let id = if id.is_empty() { format!("call_{name}") } else { id.to_string() };
            let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
            tool_calls.push(json!({
                "id": id,
                "type": openai_blocks::FUNCTION,
                "function": {
                    "name": name,
                    "arguments": serde_json::to_string(&args).unwrap_or_default()
                }
            }));
        }

        // Function response → each becomes a separate tool message
        if let Some(fr) = part.get("functionResponse").filter(|v| v.is_object()) {
            let id = fr.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let name = fr.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let id = if id.is_empty() { format!("call_{name}") } else { id.to_string() };
            let response = fr.get("response").cloned().unwrap_or_else(|| json!({}));
            let content_json = if response.is_object() {
                response.get("result").cloned().unwrap_or_else(|| Json::Object(response.as_object().unwrap().clone()))
            } else {
                response
            };
            tool_results.push(json!({
                "role": roles::TOOL,
                "tool_call_id": id,
                "content": serde_json::to_string(&content_json).unwrap_or_default()
            }));
        }
    }

    if !tool_results.is_empty() {
        if !tool_calls.is_empty() || !text_parts.is_empty() || !reasoning_content.is_empty() {
            let mut assistant_msg = json!({"role": roles::ASSISTANT});
            if !text_parts.is_empty() {
                assistant_msg["content"] = collapse(&text_parts);
            }
            if !reasoning_content.is_empty() {
                assistant_msg["reasoning_content"] = json!(reasoning_content);
            }
            if !tool_calls.is_empty() {
                assistant_msg["tool_calls"] = Json::Array(tool_calls);
            }
            tool_results.push(assistant_msg);
        }
        return Some(Json::Array(tool_results));
    }

    if !tool_calls.is_empty() {
        let mut msg = json!({"role": roles::ASSISTANT});
        if !text_parts.is_empty() {
            msg["content"] = collapse(&text_parts);
        }
        if !reasoning_content.is_empty() {
            msg["reasoning_content"] = json!(reasoning_content);
        }
        msg["tool_calls"] = Json::Array(tool_calls);
        return Some(msg);
    }

    if !text_parts.is_empty() || !reasoning_content.is_empty() {
        let mut msg = json!({"role": role});
        if !text_parts.is_empty() {
            msg["content"] = collapse(&text_parts);
        }
        if !reasoning_content.is_empty() {
            msg["reasoning_content"] = json!(reasoning_content);
        }
        return Some(msg);
    }

    None
}

/// Convert Antigravity request to OpenAI format.
pub fn antigravity_to_openai_request(
    model: &str,
    body: Json,
    stream: bool,
    _credentials: Option<&mut Credentials>,
    _options: &TranslationOptions,
) -> Json {
    let req = if body.get("request").map(|r| r.is_object()).unwrap_or(false) {
        body["request"].clone()
    } else {
        body
    };
    let mut result = json!({
        "model": model,
        "messages": [],
        "stream": stream
    });

    if let Some(config) = req.get("generationConfig").filter(|v| v.is_object()) {
        if let Some(max_output_tokens) = config.get("maxOutputTokens").and_then(|v| v.as_i64()) {
            // Adjust max_tokens for tool calls (JS: adjustMaxTokens)
            let has_tools = req.get("tools").and_then(|t| t.as_array()).map_or(false, |t| !t.is_empty());
            let adjusted = if has_tools { max_output_tokens.max(32000) } else { max_output_tokens };
            result["max_tokens"] = json!(adjusted);
        }
        if let Some(temperature) = config.get("temperature") {
            result["temperature"] = temperature.clone();
        }
        if let Some(top_p) = config.get("topP") {
            result["top_p"] = top_p.clone();
        }
        if let Some(top_k) = config.get("topK") {
            result["top_k"] = top_k.clone();
        }
        if let Some(thinking_config) = config.get("thinkingConfig").filter(|v| v.is_object()) {
            let budget = thinking_config.get("thinkingBudget").and_then(|v| v.as_i64()).unwrap_or(0);
            if let Some(effort) = thinking::budget_to_effort(budget) {
                result["reasoning_effort"] = json!(effort);
            }
        }
    }

    if let Some(instruction) = req.get("systemInstruction").filter(|v| !v.is_null()) {
        let system_text = extract_text(instruction);
        if !system_text.is_empty() {
            result["messages"].as_array_mut().unwrap().push(json!({"role": roles::SYSTEM, "content": system_text}));
        }
    }

    if let Some(contents) = req.get("contents").and_then(|c| c.as_array()) {
        for content in contents {
            if let Some(converted) = convert_content(content) {
                if let Some(arr) = converted.as_array() {
                    result["messages"].as_array_mut().unwrap().extend(arr.iter().cloned());
                } else {
                    result["messages"].as_array_mut().unwrap().push(converted);
                }
            }
        }
    }

    if let Some(tools) = req.get("tools").and_then(|t| t.as_array()) {
        let mut result_tools: Vec<Json> = Vec::new();
        for tool in tools {
            if let Some(declarations) = tool.get("functionDeclarations").and_then(|d| d.as_array()) {
                for func in declarations {
                    let name = func.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let desc = func.get("description").and_then(|v| v.as_str()).unwrap_or("");
                    let params = func.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                    result_tools.push(json!({
                        "type": openai_blocks::FUNCTION,
                        "function": {
                            "name": name,
                            "description": desc,
                            "parameters": normalize_schema_types(&params)
                        }
                    }));
                }
            }
        }
        if !result_tools.is_empty() {
            result["tools"] = Json::Array(result_tools);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_antigravity_to_openai_request_basic() {
        let body = json!({
            "request": {
                "contents": [
                    {"role": "user", "parts": [{"text": "hello"}]},
                    {"role": "model", "parts": [{"text": "hi"}]}
                ],
                "systemInstruction": {"parts": [{"text": "be nice"}]},
                "generationConfig": {"temperature": 0.3, "maxOutputTokens": 2048}
            }
        });
        let result = antigravity_to_openai_request("gemini-2.5-pro", body, false, None, &Default::default());
        assert_eq!(result["model"], "gemini-2.5-pro");
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "be nice");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "hello");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(result["temperature"], 0.3);
    }

    #[test]
    fn test_antigravity_to_openai_tool_calls() {
        let body = json!({
            "request": {
                "contents": [{
                    "role": "model",
                    "parts": [
                        {"text": "Let me check."},
                        {"functionCall": {"name": "get_weather", "args": {"city": "NYC"}}}
                    ]
                }]
            }
        });
        let result = antigravity_to_openai_request("gemini-2.5-pro", body, true, None, &Default::default());
        let msg = &result["messages"][0];
        assert_eq!(msg["role"], "assistant");
        assert_eq!(msg["content"], "Let me check.");
        assert_eq!(msg["tool_calls"][0]["function"]["name"], "get_weather");
        assert!(msg["tool_calls"][0]["id"].as_str().unwrap().starts_with("call_"));
    }

    #[test]
    fn test_antigravity_to_openai_tool_result() {
        let body = json!({
            "request": {
                "contents": [{
                    "role": "user",
                    "parts": [{
                        "functionResponse": {"name": "get_weather", "id": "call_1", "response": {"result": "sunny"}}
                    }]
                }]
            }
        });
        let result = antigravity_to_openai_request("gemini-2.5-pro", body, true, None, &Default::default());
        let msg = &result["messages"][0];
        assert_eq!(msg["role"], "tool");
        assert_eq!(msg["tool_call_id"], "call_1");
        assert_eq!(msg["content"], "\"sunny\"");
    }

    #[test]
    fn test_antigravity_to_openai_thinking_content() {
        let body = json!({
            "request": {
                "contents": [{
                    "role": "model",
                    "parts": [
                        {"thought": true, "text": "Hmm..."},
                        {"text": "Answer."}
                    ]
                }]
            }
        });
        let result = antigravity_to_openai_request("gemini-2.5-pro", body, true, None, &Default::default());
        let msg = &result["messages"][0];
        assert_eq!(msg["role"], "assistant");
        assert_eq!(msg["content"], "Answer.");
        assert_eq!(msg["reasoning_content"], "Hmm...");
    }

    #[test]
    fn test_antigravity_to_openai_schema_lowercase() {
        let body = json!({
            "request": {
                "contents": [],
                "tools": [{
                    "functionDeclarations": [{
                        "name": "f",
                        "parameters": {
                            "type": "OBJECT",
                            "properties": {
                                "x": {"type": "STRING", "enumDescriptions": ["desc"]}
                            }
                        }
                    }]
                }]
            }
        });
        let result = antigravity_to_openai_request("gemini-2.5-pro", body, true, None, &Default::default());
        let params = &result["tools"][0]["function"]["parameters"];
        assert_eq!(params["type"], "object");
        assert_eq!(params["properties"]["x"]["type"], "string");
        assert!(params["properties"]["x"].get("enumDescriptions").is_none());
    }
}
