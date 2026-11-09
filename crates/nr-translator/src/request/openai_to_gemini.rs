//! OpenAI to Gemini request translator. Port of open-sse/translator/request/openai-to-gemini.js.
//! Also used as the pivot for openai→vertex (Gemini envelope) — see openai_to_vertex.

use nr_core::{Credentials, Json};
use serde_json::json;

use crate::formats::gemini as gf;
use crate::schema::{openai_blocks, openai_finish, roles};
use crate::TranslationOptions;

/// Convert OpenAI request to Gemini format.
pub fn openai_to_gemini_request(
    model: &str,
    body: Json,
    _stream: bool,
    _credentials: Option<&mut Credentials>,
    _options: &TranslationOptions,
) -> Json {
    let mut result = json!({
        "model": model,
        "contents": [],
        "safetySettings": gf::default_safety_settings()
    });

    // Generation config
    let mut generation_config = serde_json::Map::new();
    if let Some(temp) = body.get("temperature").and_then(|v| v.as_f64()) {
        generation_config.insert("temperature".to_string(), json!(temp));
    }
    if let Some(top_p) = body.get("top_p").and_then(|v| v.as_f64()) {
        generation_config.insert("topP".to_string(), json!(top_p));
    }
    if let Some(max_tokens) = body.get("max_tokens").and_then(|v| v.as_i64()) {
        generation_config.insert("maxOutputTokens".to_string(), json!(max_tokens));
    }
    if !generation_config.is_empty() {
        result["generationConfig"] = Json::Object(generation_config);
    }

    // System message → systemInstruction
    let mut system_parts: Vec<Json> = Vec::new();
    if let Some(messages) = body.get("messages").and_then(|m| m.as_array()) {
        for msg in messages {
            if msg.get("role").and_then(|v| v.as_str()) != Some(roles::SYSTEM) {
                continue;
            }
            let text = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
            if !text.is_empty() {
                system_parts.push(json!({"text": text}));
            }
        }
    }
    if !system_parts.is_empty() {
        result["systemInstruction"] = json!({
            "role": "user",
            "parts": system_parts
        });
    }

    // Convert messages to contents
    let mut contents: Vec<Json> = Vec::new();
    if let Some(messages) = body.get("messages").and_then(|m| m.as_array()) {
        for msg in messages {
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
            if role == roles::SYSTEM {
                continue; // already in systemInstruction
            }
            let g_role = if role == roles::ASSISTANT { "model" } else { "user" };
            let parts = gf::convert_openai_content_to_parts(&msg.get("content").cloned().unwrap_or(Json::Null));
            if !parts.is_empty() {
                contents.push(json!({"role": g_role, "parts": parts}));
            }
        }
    }
    result["contents"] = Json::Array(contents);

    // Tools
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()).filter(|v| !v.is_empty()) {
        let mut declarations = Vec::new();
        for tool in tools {
            let func = tool.get("function").unwrap_or(tool);
            let name = func.get("name").and_then(|v| v.as_str()).unwrap_or("unknown");
            let desc = func.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let parameters = func.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            declarations.push(json!({
                "name": name,
                "description": desc,
                "parameters": gf::clean_json_schema_for_antigravity(parameters)
            }));
        }
        if !declarations.is_empty() {
            result["tools"] = json!([{"functionDeclarations": declarations}]);
        }
    }

    // Tool choice: ANY/required → allowed_function_names
    if let Some(choice) = body.get("tool_choice").and_then(|v| v.as_str()) {
        if choice == "required" {
            result["toolConfig"] = json!({
                "functionCallingConfig": {"mode": "ANY"}
            });
        } else if choice == "none" {
            result["toolConfig"] = json!({
                "functionCallingConfig": {"mode": "NONE"}
            });
        }
    } else if let Some(obj) = body.get("tool_choice").filter(|v| v.is_object()) {
        let function = obj.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str());
        if let Some(function) = function {
            result["toolConfig"] = json!({
                "functionCallingConfig": {
                    "mode": "ANY",
                    "allowedFunctionNames": [function]
                }
            });
        }
    }

    // Response format: JSON schema / json_object
    if let Some(fmt) = body.get("response_format") {
        if fmt.get("type").and_then(|v| v.as_str()) == Some("json_object") {
            result["generationConfig"]["responseMimeType"] = json!("application/json");
        } else if fmt.get("type").and_then(|v| v.as_str()) == Some("json_schema") {
            result["generationConfig"]["responseMimeType"] = json!("application/json");
            if let Some(schema) = fmt.get("json_schema").and_then(|s| s.get("schema")) {
                result["generationConfig"]["responseSchema"] = gf::clean_json_schema_for_antigravity(schema.clone());
            }
        }
    }

    result
}

/// Convert a Gemini response chunk to OpenAI format (non-streaming aggregator helper).
pub fn gemini_completion_to_openai(response: &Json, model: &str) -> Json {
    let text: String = response
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();

    let finish_reason = response
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .and_then(|c| c.get("finishReason"))
        .and_then(|v| v.as_str())
        .map(|r| match r {
            "MAX_TOKENS" => openai_finish::LENGTH,
            "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" => openai_finish::CONTENT_FILTER,
            _ => openai_finish::STOP,
        })
        .unwrap_or(openai_finish::STOP);

    json!({
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": roles::ASSISTANT, "content": text},
            "finish_reason": finish_reason
        }]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_openai_to_gemini_request_basic() {
        let body = json!({
            "messages": [
                {"role": "system", "content": "be helpful"},
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "hi"},
                {"role": "user", "content": "ok"}
            ],
            "temperature": 0.5,
            "max_tokens": 4096
        });
        let result = openai_to_gemini_request("gemini-2.5-flash", body, true, None, &Default::default());
        assert_eq!(result["systemInstruction"]["parts"][0]["text"], "be helpful");
        let contents = result["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "hello");
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(result["generationConfig"]["temperature"], 0.5);
        assert_eq!(result["generationConfig"]["maxOutputTokens"], 4096);
    }

    #[test]
    fn test_openai_to_gemini_request_tools() {
        let body = json!({
            "messages": [{"role": "user", "content": "weather?"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get weather",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
                }
            }],
            "tool_choice": "required"
        });
        let result = openai_to_gemini_request("gemini-2.5-flash", body, true, None, &Default::default());
        let declarations = &result["tools"][0]["functionDeclarations"];
        assert_eq!(declarations[0]["name"], "get_weather");
        assert!(declarations[0]["parameters"].get("type").is_some());
        assert_eq!(result["toolConfig"]["functionCallingConfig"]["mode"], "ANY");
    }

    #[test]
    fn test_openai_to_gemini_request_tool_choice_named() {
        let body = json!({
            "messages": [{"role": "user", "content": "hi"}],
            "tool_choice": {"type": "function", "function": {"name": "f"}}
        });
        let result = openai_to_gemini_request("gemini-2.5-flash", body, true, None, &Default::default());
        assert_eq!(result["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"][0], "f");
    }

    #[test]
    fn test_openai_to_gemini_request_json_schema() {
        let body = json!({
            "messages": [{"role": "user", "content": "hello"}],
            "response_format": {
                "type": "json_schema",
                "json_schema": {"schema": {"type": "object", "properties": {"x": {"type": "string"}}}}
            }
        });
        let result = openai_to_gemini_request("gemini-2.5-flash", body, false, None, &Default::default());
        assert_eq!(result["generationConfig"]["responseMimeType"], "application/json");
        assert!(result["generationConfig"]["responseSchema"].get("properties").is_some());
    }

    #[test]
    fn test_gemini_completion_to_openai() {
        let resp = json!({
            "candidates": [{
                "content": {"parts": [{"text": "hello"}, {"text": " world"}]},
                "finishReason": "STOP"
            }]
        });
        let result = gemini_completion_to_openai(&resp, "gemini-2.5-flash");
        assert_eq!(result["choices"][0]["message"]["content"], "hello world");
        assert_eq!(result["choices"][0]["finish_reason"], "stop");
    }
}
