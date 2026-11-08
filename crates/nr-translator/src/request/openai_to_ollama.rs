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
    _stream: bool,
    _credentials: Option<&mut Credentials>,
    _options: &TranslationOptions,
) -> Json {
    let mut result = serde_json::json!({
        "model": model,
        "messages": normalize_messages(&body),
        "stream": true  // TODO: use stream param
    });

    // Temperature
    if let Some(temp) = body.get("temperature").and_then(|v| v.as_f64()) {
        result["options"] = serde_json::json!({ "temperature": temp });
    }

    // Max tokens (Ollama uses num_predict)
    if let Some(max_tokens) = body.get("max_tokens").and_then(|v| v.as_u64()) {
        result["options"]["num_predict"] = Json::from(max_tokens as i64);
    }

    // Top_p
    if let Some(top_p) = body.get("top_p").and_then(|v| v.as_f64()) {
        result["options"]["top_p"] = Json::from(top_p);
    }

    // Tools
    if let Some(tools) = body.get("tools") {
        result["tools"] = tools.clone();
    }

    // Tool choice
    if let Some(tool_choice) = body.get("tool_choice") {
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
            if content.is_null() {
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

        if content.is_null() && role != roles::ASSISTANT {
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

    Json::Null
}

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
