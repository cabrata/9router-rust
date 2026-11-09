//! OpenAI to Ollama request translator. Port of open-sse/translator/request/openai-to-ollama.js.

use nr_core::{Credentials, Json};
use crate::TranslationOptions;
use crate::concerns::image::parse_data_uri;
use crate::concerns::json::safe_parse_json;
use crate::schema::{roles, openai_blocks};

/// Convert OpenAI request to Ollama format.
pub fn openai_to_ollama_request(
    model: &str,
    body: Json,
    stream: bool,
    _credentials: Option<&mut Credentials>,
    _options: &TranslationOptions,
) -> Json {
    let mut result = serde_json::json!({
        "model": model,
        "messages": normalize_messages(&body),
        "stream": stream
    });

    // options is shared by temperature/num_predict/top_p; create it once, keep keys.
    let mut options = serde_json::Map::new();
    if let Some(temp) = body.get("temperature").filter(|v| !v.is_null()) {
        options.insert("temperature".into(), temp.clone());
    }
    // Ollama calls max_tokens `num_predict`.
    if let Some(max_tokens) = body.get("max_tokens").filter(|v| !v.is_null()) {
        options.insert("num_predict".into(), max_tokens.clone());
    }
    if let Some(top_p) = body.get("top_p").filter(|v| !v.is_null()) {
        options.insert("top_p".into(), top_p.clone());
    }
    if !options.is_empty() {
        result["options"] = Json::Object(options);
    }

    // Ollama accepts tools/tool_choice in OpenAI shape.
    if let Some(tools) = body.get("tools").filter(|v| v.is_array()) {
        result["tools"] = tools.clone();
    }
    if let Some(tool_choice) = body.get("tool_choice").filter(|v| !v.is_null()) {
        result["tool_choice"] = tool_choice.clone();
    }

    result
}

/// Normalize messages to Ollama format.
fn normalize_messages(body: &Json) -> Json {
    let mut result = Vec::new();
    let messages = match body.get("messages").and_then(|v| v.as_array()) {
        Some(msgs) => msgs.clone(),
        None => return Json::Array(Vec::new()),
    };

    // Build tool call map
    let mut tool_call_map = std::collections::HashMap::new();
    for msg in &messages {
        if msg.get("role").and_then(|v| v.as_str()) == Some(roles::ASSISTANT) {
            if let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tool_calls {
                    if let (Some(id), Some(func)) = (tc.get("id"), tc.get("function")) {
                        if let (Some(id_str), Some(name)) = (id.as_str(), func.get("name").and_then(|v| v.as_str())) {
                            tool_call_map.insert(id_str.to_string(), name.to_string());
                        }
                    }
                }
            }
        }
    }

    // Convert messages
    for msg in messages {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");

        if role == "tool" {
            let content_val = msg.get("content").unwrap_or(&Json::Null);
            let content = normalize_content(content_val);
            // Skip empty tool results (JS `if (!toolResult) continue`).
            if content.as_str().map(|s| s.is_empty()).unwrap_or(true) {
                continue;
            }

            let tool_call_id = msg.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("");
            let tool_name = tool_call_map
                .get(tool_call_id)
                .map(|s| s.as_str())
                .or_else(|| msg.get("name").and_then(|v| v.as_str()))
                .unwrap_or("unknown_tool");

            let out = serde_json::json!({
                "role": "tool",
                "tool_name": tool_name,
                "content": content
            });
            result.push(out);
            continue;
        }

        if role == roles::ASSISTANT && msg.get("tool_calls").is_some() {
            let content_val = msg.get("content").unwrap_or(&Json::Null);
            let content = normalize_content(content_val).as_str().unwrap_or("").to_string();
            let mut out = serde_json::json!({
                "role": "assistant",
                "content": content
            });

            if let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                let mut tc_vec = Vec::new();
                for tc in tool_calls {
                    let func = tc.get("function").unwrap_or(&Json::Null);
                    let args = func.get("arguments").cloned().unwrap_or(Json::Null);
                    let parsed_args = match args.as_str() {
                        Some(s) => safe_parse_json(s, Json::Object(Default::default())),
                        _ => args,
                    };
                    tc_vec.push(serde_json::json!({
                        "type": "function",
                        "function": {
                            "index": tc.get("index").cloned().unwrap_or(Json::from(0)),
                            "name": func.get("name").cloned().unwrap_or(Json::Null),
                            "arguments": parsed_args
                        }
                    }));
                }
                out["tool_calls"] = Json::Array(tc_vec);
            }

            result.push(out);
            continue;
        }

        let content_val = msg.get("content").unwrap_or(&Json::Null);
        let content = normalize_content(content_val);
        let images = extract_images(content_val);

        let empty = content.as_str().map(|s| s.is_empty()).unwrap_or(true);
        if empty && role != roles::ASSISTANT {
            continue;
        }

        let mut out = serde_json::json!({
            "role": role,
            "content": content
        });

        if !images.is_empty() {
            out["images"] = Json::Array(images.iter().map(|s| Json::String(s.clone())).collect());
        }

        result.push(out);
    }

    Json::Array(result)
}

/// Ollama only accepts string content; text blocks are joined with newlines.
fn normalize_content(content: &Json) -> Json {
    if let Some(s) = content.as_str() {
        return Json::String(s.to_string());
    }

    if let Some(arr) = content.as_array() {
        let text_parts: Vec<String> = arr
            .iter()
            .filter(|block| {
                block.get("type").and_then(|v| v.as_str()) == Some(openai_blocks::TEXT)
                    && block.get("text").is_some()
            })
            .filter_map(|block| block.get("text").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .collect();
        return Json::String(text_parts.join("\n"));
    }

    Json::String(String::new())
}

/// Ollama wants raw base64 in `images[]`, not OpenAI data URIs.
fn extract_images(content: &Json) -> Vec<String> {
    let mut images = Vec::new();

    if let Some(arr) = content.as_array() {
        for block in arr {
            if block.get("type").and_then(|v| v.as_str()) != Some(openai_blocks::IMAGE_URL) {
                continue;
            }

            let url = block
                .get("image_url")
                .and_then(|v| v.as_str())
                .or_else(|| block.get("image_url").and_then(|v| v.get("url")).and_then(|v| v.as_str()));

            if let Some(url_str) = url {
                if let Some(parsed) = parse_data_uri(url_str) {
                    images.push(parsed.base64);
                }
            }
        }
    }

    images
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req(body: Json, stream: bool) -> Json {
        openai_to_ollama_request("llama3", body, stream, None, &Default::default())
    }

    #[test]
    fn test_stream_flag_and_options() {
        let body = json!({"messages": [{"role": "user", "content": "hi"}], "temperature": 0.5, "max_tokens": 42, "top_p": 0.9});
        let result = req(body, false);
        assert_eq!(result["stream"], false);
        assert_eq!(result["options"]["temperature"], 0.5);
        assert_eq!(result["options"]["num_predict"], 42);
        assert_eq!(result["options"]["top_p"], 0.9);
    }

    #[test]
    fn test_no_options_when_absent() {
        let result = req(json!({"messages": [{"role": "user", "content": "hi"}]}), true);
        assert!(result.get("options").is_none());
        assert_eq!(result["stream"], true);
    }

    #[test]
    fn test_content_array_flattened_to_string() {
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "a"}, {"type": "text", "text": "b"}
        ]}]});
        let result = req(body, false);
        assert_eq!(result["messages"][0]["content"], "a\nb");
    }

    #[test]
    fn test_tool_call_arguments_parsed_and_index_defaulted() {
        let body = json!({"messages": [{"role": "assistant", "content": null, "tool_calls": [
            {"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{\"a\":1}"}}
        ]}]});
        let result = req(body, false);
        assert_eq!(result["messages"][0]["tool_calls"][0]["function"]["arguments"]["a"], 1);
        assert_eq!(result["messages"][0]["tool_calls"][0]["function"]["index"], 0);
        assert_eq!(result["messages"][0]["content"], "");
    }

    #[test]
    fn test_tool_message_maps_tool_name() {
        let body = json!({"messages": [
            {"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "search", "arguments": "{}"}}]},
            {"role": "tool", "tool_call_id": "c1", "content": "out"}
        ]});
        let result = req(body, false);
        assert_eq!(result["messages"][1]["tool_name"], "search");
        assert_eq!(result["messages"][1]["content"], "out");
    }

    #[test]
    fn test_empty_messages_and_tool_results_dropped() {
        let body = json!({"messages": [
            {"role": "user", "content": "ok"},
            {"role": "user", "content": ""},
            {"role": "tool", "tool_call_id": "x", "content": ""}
        ]});
        let result = req(body, false);
        assert_eq!(result["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_image_data_uri_becomes_raw_base64() {
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "look"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}}
        ]}]});
        let result = req(body, false);
        assert_eq!(result["messages"][0]["content"], "look");
        assert_eq!(result["messages"][0]["images"][0], "QUJD");
    }
}
