//! OpenAI to CommandCode request translator. Port of open-sse/translator/request/openai-to-commandcode.js.

use nr_core::{Credentials, Json};
use serde_json::json;
use uuid::Uuid;

use crate::concerns::image;
use crate::schema::{claude_blocks, openai_blocks, roles};
use crate::TranslationOptions;

const COMMAND_CODE_WORKING_DIR: &str = "/workspace";
const COMMAND_CODE_MAX_TOKENS: i64 = 64000;

// shortcut: deterministic per-session uuid; JS toCommandCodeSessionUuid in shared/commandCodeHeaders.js.
fn to_command_code_session_uuid(session_id: Option<&str>) -> String {
    let input = session_id.unwrap_or("").to_string();
    Uuid::new_v5(&Uuid::NAMESPACE_DNS, input.as_bytes()).to_string()
}

fn flatten_text(content: &Json) -> String {
    if content.is_null() {
        return String::new();
    }
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(arr) = content.as_array() {
        let parts: Vec<String> = arr
            .iter()
            .filter_map(|p| {
                if let Some(s) = p.as_str() {
                    Some(s.to_string())
                } else if p.is_object() {
                    p.get("text").and_then(|t| t.as_str()).map(|s| s.to_string())
                } else {
                    None
                }
            })
            .collect();
        return parts.join("\n");
    }
    content.to_string()
}

fn to_native_image_block(part: &Json) -> Option<Json> {
    let block_type = part.get("type").and_then(|v| v.as_str()).unwrap_or("");

    if block_type == openai_blocks::IMAGE_URL {
        let url = match part.get("image_url") {
            Some(v) if v.is_string() => v.as_str().unwrap_or(""),
            Some(v) => v.get("url").and_then(|u| u.as_str()).unwrap_or(""),
            _ => "",
        };
        let parsed = image::parse_data_uri(url)?;
        return Some(json!({
            "type": openai_blocks::IMAGE,
            "image": image::encode_data_uri(&parsed.mime_type, &parsed.base64),
            "mimeType": parsed.mime_type,
            "mediaType": parsed.mime_type,
        }));
    }

    if block_type == openai_blocks::IMAGE || block_type == claude_blocks::IMAGE {
        if let Some(img) = part.get("image").and_then(|v| v.as_str()).filter(|s| s.starts_with("data:")) {
            let parsed = image::parse_data_uri(img);
            let mime = part.get("mimeType").and_then(|v| v.as_str()).unwrap_or_else(|| parsed.as_ref().map(|p| p.mime_type.as_str()).unwrap_or("image/png"));
            return Some(json!({
                "type": openai_blocks::IMAGE,
                "image": img,
                "mimeType": mime,
                "mediaType": mime,
            }));
        }
        let source = part.get("source").filter(|v| v.is_object());
        if let Some(source) = source {
            if source.get("type").and_then(|v| v.as_str()) == Some("base64") {
                if let Some(data) = source.get("data").and_then(|v| v.as_str()) {
                    let mime = source.get("media_type").and_then(|v| v.as_str()).unwrap_or("image/png");
                    return Some(json!({
                        "type": openai_blocks::IMAGE,
                        "image": image::encode_data_uri(mime, data),
                        "mimeType": mime,
                        "mediaType": mime,
                    }));
                }
            }
        }
    }
    None
}

fn to_content_blocks(content: &Json) -> Vec<Json> {
    if content.is_null() {
        return vec![json!({"type": openai_blocks::TEXT, "text": ""})];
    }
    if let Some(s) = content.as_str() {
        return vec![json!({"type": openai_blocks::TEXT, "text": s})];
    }
    if let Some(arr) = content.as_array() {
        let mut blocks = Vec::new();
        for part in arr {
            if let Some(s) = part.as_str() {
                blocks.push(json!({"type": openai_blocks::TEXT, "text": s}));
                continue;
            }
            if part.is_object() {
                if part.get("type").and_then(|v| v.as_str()) == Some(openai_blocks::TEXT) {
                    if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                        blocks.push(json!({"type": openai_blocks::TEXT, "text": text}));
                    }
                    continue;
                }
                if let Some(image) = to_native_image_block(part) {
                    blocks.push(image);
                    continue;
                }
                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                    blocks.push(json!({"type": openai_blocks::TEXT, "text": text}));
                }
            }
        }
        if !blocks.is_empty() {
            return blocks;
        }
    }
    vec![json!({"type": openai_blocks::TEXT, "text": content.to_string()})]
}

fn safe_parse_json(s: &Json) -> Json {
    if let Some(parsed) = s.as_object() {
        return Json::Object(parsed.clone());
    }
    if let Some(arr) = s.as_array() {
        return Json::Array(arr.clone());
    }
    let Some(s) = s.as_str() else { return json!({}) };
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return json!({});
    }
    match serde_json::from_str::<Json>(trimmed) {
        Ok(Json::Object(_)) | Ok(Json::Array(_)) => serde_json::from_str(trimmed).unwrap_or_else(|_| json!({"value": s})),
        Ok(other) => json!({"value": other}),
        Err(_) => json!({"raw": s}),
    }
}

fn convert_messages(messages: &[Json]) -> (Vec<Json>, String) {
    let mut out = Vec::new();
    let mut system_texts = Vec::new();
    let mut tool_call_id_to_name: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    for m in messages {
        if m.get("role").and_then(|v| v.as_str()) == Some(roles::ASSISTANT) {
            if let Some(tool_calls) = m.get("tool_calls").and_then(|t| t.as_array()) {
                for tc in tool_calls {
                    let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    let name = tc.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()).or_else(|| tc.get("name").and_then(|v| v.as_str()));
                    if !id.is_empty() {
                        if let Some(name) = name {
                            tool_call_id_to_name.insert(id.to_string(), name.to_string());
                        }
                    }
                }
            }
        }
    }

    for m in messages {
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("");

        if role == roles::SYSTEM {
            let t = flatten_text(&m.get("content").cloned().unwrap_or(Json::Null));
            if !t.is_empty() {
                system_texts.push(t);
            }
            continue;
        }

        if role == roles::TOOL {
            let content = m.get("content").cloned().unwrap_or(Json::Null);
            let value = if content.is_string() {
                content.as_str().unwrap_or("").to_string()
            } else {
                flatten_text(&content)
            };
            let is_error = m.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false)
                || m.get("status").and_then(|v| v.as_str()) == Some("error");
            let tool_name = m
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| m.get("tool_call_id").and_then(|v| v.as_str()).and_then(|id| tool_call_id_to_name.get(id).cloned()))
                .unwrap_or_else(|| "unknown".to_string());
            out.push(json!({
                "role": roles::TOOL,
                "content": [{
                    "type": "tool-result",
                    "toolCallId": m.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or(""),
                    "toolName": tool_name,
                    "output": {"type": if is_error { "error-text" } else { "text" }, "value": value}
                }]
            }));
            continue;
        }

        if role == roles::ASSISTANT {
            let mut blocks = Vec::new();
            let rc = m.get("reasoning_content").or_else(|| m.get("thought")).or_else(|| m.get("reasoning")).and_then(|v| v.as_str());
            if let Some(rc) = rc.filter(|s| !s.trim().is_empty()) {
                blocks.push(json!({"type": "reasoning", "text": rc}));
            }
            let text = flatten_text(&m.get("content").cloned().unwrap_or(Json::Null));
            if !text.is_empty() {
                blocks.push(json!({"type": openai_blocks::TEXT, "text": text}));
            }
            if let Some(tool_calls) = m.get("tool_calls").and_then(|t| t.as_array()) {
                for tc in tool_calls {
                    let fn_obj = tc.get("function").filter(|f| f.is_object()).cloned().unwrap_or_else(|| json!({}));
                    let name = fn_obj.get("name").and_then(|v| v.as_str()).or_else(|| tc.get("name").and_then(|v| v.as_str())).unwrap_or("");
                    let arguments = fn_obj.get("arguments").cloned().unwrap_or_else(|| tc.get("input").cloned().unwrap_or(Json::Null));
                    blocks.push(json!({
                        "type": "tool-call",
                        "toolCallId": tc.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                        "toolName": name,
                        "input": safe_parse_json(&arguments)
                    }));
                }
            }
            if blocks.is_empty() {
                blocks.push(json!({"type": openai_blocks::TEXT, "text": ""}));
            }
            out.push(json!({"role": roles::ASSISTANT, "content": blocks}));
            continue;
        }

        out.push(json!({"role": roles::USER, "content": to_content_blocks(&m.get("content").cloned().unwrap_or(Json::Null))}));
    }

    (out, system_texts.join("\n\n"))
}

fn convert_tools(tools: Option<&Json>) -> Option<Json> {
    let tools = tools?.as_array()?;
    if tools.is_empty() {
        return None;
    }
    let mut result = Vec::new();
    for t in tools {
        if t.get("type").and_then(|v| v.as_str()) == Some(openai_blocks::FUNCTION) && t.get("function").is_some() {
            let func = t["function"].clone();
            result.push(json!({
                "name": func.get("name").cloned().unwrap_or(Json::Null),
                "description": func.get("description").cloned().unwrap_or(Json::Null),
                "input_schema": func.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object"}))
            }));
        } else if let Some(name) = t.get("name").and_then(|v| v.as_str()) {
            let input_schema = t.get("input_schema").or_else(|| t.get("parameters")).cloned();
            result.push(json!({
                "name": name,
                "description": t.get("description").cloned().unwrap_or(Json::Null),
                "input_schema": input_schema.unwrap_or_else(|| json!({"type": "object"}))
            }));
        }
    }
    if result.is_empty() { None } else { Some(Json::Array(result)) }
}

/// Convert OpenAI request to CommandCode format.
pub fn openai_to_commandcode_request(
    model: &str,
    body: Json,
    stream: bool,
    _credentials: Option<&mut Credentials>,
    _options: &TranslationOptions,
) -> Json {
    let msgs = body.get("messages").and_then(|m| m.as_array()).map(|v| v.as_slice()).unwrap_or(&[]);
    let (messages, system) = convert_messages(msgs);
    let mut params = json!({
        "model": model,
        "messages": messages,
        "stream": stream,
        "max_tokens": body.get("max_tokens").or_else(|| body.get("max_output_tokens")).and_then(|v| v.as_i64()).unwrap_or(COMMAND_CODE_MAX_TOKENS),
        "temperature": body.get("temperature").and_then(|v| v.as_f64()).unwrap_or(0.3),
    });
    if !system.is_empty() {
        params["system"] = json!(system);
    }
    if let Some(tools) = convert_tools(body.get("tools")) {
        params["tools"] = tools;
    }
    if let Some(top_p) = body.get("top_p") {
        params["top_p"] = top_p.clone();
    }

    let effort = body.get("reasoning_effort").cloned().unwrap_or(Json::Null);
    let effort = if !effort.is_null() {
        Some(effort)
    } else {
        body.get("output_config").and_then(|oc| oc.get("effort")).cloned().or_else(|| {
            body.get("reasoning").filter(|r| r.is_object()).and_then(|r| r.get("effort")).cloned()
        })
    };
    if let Some(effort) = effort.filter(|v| v.is_string()) {
        params["reasoning_effort"] = effort;
    }

    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    json!({
        "threadId": to_command_code_session_uuid(None),
        "memory": null,
        "taste": null,
        "skills": null,
        "permissionMode": "standard",
        "config": {
            "workingDir": COMMAND_CODE_WORKING_DIR,
            "date": today,
            "environment": std::env::consts::OS,
            "structure": [],
            "isGitRepo": false,
            "currentBranch": "",
            "mainBranch": "",
            "gitStatus": "",
            "recentCommits": []
        },
        "params": params
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_openai_to_commandcode_system_and_tools() {
        let body = json!({
            "messages": [
                {"role": "system", "content": "Be concise."},
                {"role": "user", "content": "hi"}
            ],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "f",
                    "description": "d",
                    "parameters": {"type": "object", "properties": {}}
                }
            }],
            "max_tokens": 1024
        });
        let result = openai_to_commandcode_request("cc-model", body, true, None, &Default::default());
        let params = &result["params"];
        assert_eq!(params["system"], "Be concise.");
        assert_eq!(params["model"], "cc-model");
        assert_eq!(params["stream"], true);
        assert_eq!(params["max_tokens"], 1024);
        assert_eq!(params["tools"][0]["name"], "f");
        assert_eq!(result["config"]["workingDir"], COMMAND_CODE_WORKING_DIR);
    }

    #[test]
    fn test_openai_to_commandcode_assistant_tool_call() {
        let body = json!({
            "messages": [
                {"role": "assistant", "tool_calls": [
                    {"id": "t1", "function": {"name": "search", "arguments": "{\"q\":\"x\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "t1", "content": "found"}
            ]
        });
        let result = openai_to_commandcode_request("cc-model", body, false, None, &Default::default());
        let messages = result["params"]["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["content"][0]["type"], "tool-call");
        assert_eq!(messages[0]["content"][0]["toolName"], "search");
        assert_eq!(messages[0]["content"][0]["input"]["q"], "x");
        assert_eq!(messages[1]["content"][0]["type"], "tool-result");
        // tool name resolved from assistant turn's tool_calls
        assert_eq!(messages[1]["content"][0]["toolName"], "search");
    }

    #[test]
    fn test_openai_to_commandcode_default_tokens_and_temp() {
        let body = json!({"messages": [{"role": "user", "content": "hi"}]});
        let result = openai_to_commandcode_request("cc", body, true, None, &Default::default());
        assert_eq!(result["params"]["max_tokens"], COMMAND_CODE_MAX_TOKENS);
        assert_eq!(result["params"]["temperature"], 0.3);
        assert!(result["params"].get("system").is_none());
    }
}
