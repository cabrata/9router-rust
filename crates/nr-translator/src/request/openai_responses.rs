//! OpenAI Responses API ↔ Chat Completions request translator.
//! Port of open-sse/translator/request/openai-responses.js.

use std::collections::HashSet;

use nr_core::{Credentials, Json};
use serde_json::json;

use crate::formats::responses_api::{
    clamp_responses_call_id, coerce_responses_arguments, coerce_responses_output,
    normalize_responses_input,
};
use crate::schema::{openai_blocks, responses_items, roles};
use crate::TranslationOptions;

const MAX_TOOL_NAME_LEN: usize = 128;

fn extract_reasoning_text(item: &Json) -> String {
    if let Some(summary) = item.get("summary").and_then(|s| s.as_array()) {
        let txt = summary
            .iter()
            .filter_map(|s| s.get("text").and_then(|t| t.as_str()))
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if !txt.is_empty() {
            return txt;
        }
    }
    if let Some(content) = item.get("content").and_then(|c| c.as_array()) {
        let txt = content
            .iter()
            .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if !txt.is_empty() {
            return txt;
        }
    }
    String::new()
}

fn normalize_tool_parameters(params: Option<&Json>) -> Json {
    let mut p = params.cloned().unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    if p.get("type").and_then(|v| v.as_str()) == Some("object") && p.get("properties").is_none() {
        if let Some(obj) = p.as_object_mut() {
            obj.insert("properties".to_string(), json!({}));
        }
    }
    p
}

/// Convert OpenAI Responses API request → OpenAI Chat Completions format.
pub fn openai_responses_to_openai_request(
    _model: &str,
    body: Json,
    _stream: bool,
    _credentials: Option<&mut Credentials>,
    _options: &TranslationOptions,
) -> Json {
    let Some(input) = body.get("input") else {
        return body;
    };
    let Some(input_items) = normalize_responses_input(input) else {
        return body;
    };

    let mut result = body.clone();
    let mut messages: Vec<Json> = Vec::new();

    // Instructions → system message
    if let Some(instructions) = body.get("instructions").and_then(|i| i.as_str()) {
        if !instructions.is_empty() {
            messages.push(json!({"role": roles::SYSTEM, "content": instructions}));
        }
    }

    let mut current_assistant_msg: Option<Json> = None;
    let mut pending_tool_results: Vec<Json> = Vec::new();
    let mut pending_reasoning = String::new();
    let mut pending_reasoning_encrypted = String::new();
    let mut additional_tools: Vec<Json> = Vec::new();
    let mut custom_tool_names: HashSet<String> = HashSet::new();

    fn attach_pending_reasoning(msg: &mut Json, pending_reasoning: &mut String, pending_reasoning_encrypted: &mut String) {
        if !pending_reasoning.is_empty() {
            msg["reasoning_content"] = json!(pending_reasoning);
            pending_reasoning.clear();
        }
        if !pending_reasoning_encrypted.is_empty() {
            msg["encrypted_content"] = json!(pending_reasoning_encrypted);
            pending_reasoning_encrypted.clear();
        }
    };

    for item in input_items {
        let item_type = item
            .get("type")
            .and_then(|v| v.as_str())
            .or_else(|| item.get("role").map(|_| responses_items::MESSAGE));

        match item_type {
            Some(responses_items::MESSAGE) => {
                if let Some(mut m) = current_assistant_msg.take() {
                    messages.push(m);
                }
                if !pending_tool_results.is_empty() {
                    messages.extend(pending_tool_results.drain(..));
                }

                let content = if let Some(arr) = item.get("content").and_then(|c| c.as_array()) {
                    let mapped: Vec<Json> = arr
                        .iter()
                        .map(|c| {
                            match c.get("type").and_then(|v| v.as_str()) {
                                Some(responses_items::INPUT_TEXT) | Some(responses_items::OUTPUT_TEXT) => {
                                    json!({"type": openai_blocks::TEXT, "text": c.get("text").and_then(|v| v.as_str()).unwrap_or("")})
                                }
                                Some(responses_items::INPUT_IMAGE) => {
                                    let url = c.get("image_url").or_else(|| c.get("file_id")).and_then(|v| v.as_str()).unwrap_or("");
                                    let detail = c.get("detail").and_then(|v| v.as_str()).unwrap_or("auto");
                                    json!({"type": openai_blocks::IMAGE_URL, "image_url": {"url": url, "detail": detail}})
                                }
                                _ => c.clone(),
                            }
                        })
                        .collect();
                    Json::Array(mapped)
                } else {
                    item.get("content").cloned().unwrap_or(Json::Null)
                };

                let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("user");
                let mut msg = json!({"role": role, "content": content});
                if role == roles::ASSISTANT {
                    attach_pending_reasoning(&mut msg, &mut pending_reasoning, &mut pending_reasoning_encrypted);
                } else {
                    pending_reasoning.clear();
                    pending_reasoning_encrypted.clear();
                }
                messages.push(msg);
            }
            Some(responses_items::FUNCTION_CALL) | Some(responses_items::CUSTOM_TOOL_CALL) => {
                if current_assistant_msg.is_none() {
                    let mut m = json!({
                        "role": roles::ASSISTANT,
                        "content": null,
                        "tool_calls": []
                    });
                    attach_pending_reasoning(&mut m, &mut pending_reasoning, &mut pending_reasoning_encrypted);
                    current_assistant_msg = Some(m);
                }
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if name.trim().is_empty() {
                    continue;
                }
                if item_type == Some(responses_items::CUSTOM_TOOL_CALL) {
                    custom_tool_names.insert(name.to_string());
                }
                let tool_input = if item_type == Some(responses_items::CUSTOM_TOOL_CALL) {
                    match item.get("input") {
                        Some(Json::String(s)) => json!({"input": s}),
                        Some(other) => json!({"input": other.to_string()}),
                        None => json!({"input": ""}),
                    }
                } else {
                    item.get("arguments").cloned().unwrap_or_else(|| json!({}))
                };
                let arguments_str = if tool_input.is_string() {
                    tool_input.as_str().unwrap().to_string()
                } else {
                    serde_json::to_string(&tool_input).unwrap_or_else(|_| "{}".to_string())
                };

                let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                current_assistant_msg.as_mut().unwrap()["tool_calls"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({
                        "id": call_id,
                        "type": openai_blocks::FUNCTION,
                        "function": {
                            "name": name,
                            "arguments": arguments_str
                        }
                    }));
            }
            Some(responses_items::FUNCTION_CALL_OUTPUT) | Some(responses_items::CUSTOM_TOOL_CALL_OUTPUT) => {
                if let Some(m) = current_assistant_msg.take() {
                    messages.push(m);
                }
                if !pending_tool_results.is_empty() {
                    messages.extend(pending_tool_results.drain(..));
                }
                let output = match item.get("output") {
                    Some(Json::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                    None => String::new(),
                };
                let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                messages.push(json!({
                    "role": roles::TOOL,
                    "tool_call_id": call_id,
                    "content": output
                }));
            }
            Some(responses_items::ADDITIONAL_TOOLS) => {
                if let Some(tools) = item.get("tools").and_then(|t| t.as_array()) {
                    additional_tools.extend(tools.iter().cloned());
                }
            }
            Some(responses_items::REASONING) => {
                let txt = extract_reasoning_text(&item);
                if !txt.is_empty() {
                    if pending_reasoning.is_empty() {
                        pending_reasoning = txt;
                    } else {
                        pending_reasoning.push('\n');
                        pending_reasoning.push_str(&txt);
                    }
                }
                if let Some(enc) = item.get("encrypted_content").and_then(|v| v.as_str()) {
                    if !enc.is_empty() {
                        pending_reasoning_encrypted = enc.to_string();
                    }
                }
            }
            _ => {}
        }
    }

    if let Some(m) = current_assistant_msg.take() {
        messages.push(m);
    }
    if !pending_tool_results.is_empty() {
        messages.extend(pending_tool_results.drain(..));
    }

    // Convert tools
    let mut response_tools: Vec<Json> = Vec::new();
    if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
        response_tools.extend(tools.iter().cloned());
    }
    response_tools.extend(additional_tools);

    if !response_tools.is_empty() {
        let converted_tools: Vec<Json> = response_tools
            .into_iter()
            .filter_map(|tool| {
                if tool.get("function").is_some() {
                    return Some(tool);
                }
                let name = tool.get("name").and_then(|v| v.as_str())?;
                if name.trim().is_empty() {
                    return None;
                }
                if tool.get("type").and_then(|v| v.as_str()) == Some("custom") {
                    custom_tool_names.insert(name.to_string());
                    let desc = tool.get("description").and_then(|v| v.as_str()).unwrap_or("");
                    return Some(json!({
                        "type": openai_blocks::FUNCTION,
                        "function": {
                            "name": name,
                            "description": desc,
                            "parameters": {
                                "type": "object",
                                "properties": {
                                    "input": {
                                        "type": "string",
                                        "description": "Raw freeform input for this custom tool"
                                    }
                                },
                                "required": ["input"],
                                "additionalProperties": false
                            }
                        }
                    }));
                }
                Some(json!({
                    "type": openai_blocks::FUNCTION,
                    "function": {
                        "name": name,
                        "description": tool.get("description").cloned().unwrap_or(json!("")),
                        "parameters": normalize_tool_parameters(tool.get("parameters")),
                        "strict": tool.get("strict").cloned().unwrap_or(Json::Null)
                    }
                }))
            })
            .collect();
        result["tools"] = Json::Array(converted_tools);
    }

    if !custom_tool_names.is_empty() {
        result["_customToolNames"] = json!(custom_tool_names.into_iter().collect::<Vec<_>>());
    }

    if let Some(max_output) = result.get("max_output_tokens").cloned() {
        if result.get("max_tokens").is_none() {
            result["max_tokens"] = max_output;
        }
    }
    if let Some(effort) = result.get("reasoning").and_then(|r| r.get("effort")).cloned() {
        result["reasoning_effort"] = effort;
    }

    if let Some(obj) = result.as_object_mut() {
        for key in ["max_output_tokens", "input", "instructions", "include", "prompt_cache_key", "store", "reasoning", "client_metadata"] {
            obj.remove(key);
        }
    }
    result["messages"] = Json::Array(messages);
    result
}

fn extract_instructions_text(content: &Json) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(arr) = content.as_array() {
        return arr
            .iter()
            .filter_map(|c| {
                c.get("text")
                    .or_else(|| c.get("content"))
                    .and_then(|v| v.as_str())
            })
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
    }
    String::new()
}

fn build_reasoning_input_item(msg: &Json) -> Option<Json> {
    let encrypted = msg
        .get("encrypted_content")
        .or_else(|| msg.get("reasoning_encrypted_content"))
        .or_else(|| msg.get("reasoning").and_then(|r| r.get("encrypted_content")))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let mut summary_text = String::new();
    if let Some(rc) = msg.get("reasoning_content").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()) {
        summary_text = rc.to_string();
    } else if let Some(r) = msg.get("reasoning").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()) {
        summary_text = r.to_string();
    } else if let Some(arr) = msg.get("reasoning_details").and_then(|v| v.as_array()) {
        summary_text = arr
            .iter()
            .filter_map(|d| d.get("text").or_else(|| d.get("content")).and_then(|v| v.as_str()))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
    }

    if encrypted.is_empty() && summary_text.is_empty() {
        return None;
    }

    let mut item = json!({"type": responses_items::REASONING});
    if !summary_text.is_empty() {
        item["summary"] = json!([{"type": responses_items::SUMMARY_TEXT, "text": summary_text}]);
    }
    if !encrypted.is_empty() {
        item["encrypted_content"] = json!(encrypted);
    }
    Some(item)
}

/// Convert OpenAI Chat Completions → OpenAI Responses API format.
pub fn openai_to_openai_responses_request(
    model: &str,
    body: Json,
    _stream: bool,
    _credentials: Option<&mut Credentials>,
    _options: &TranslationOptions,
) -> Json {
    // If already in Responses API format, pass through with field normalization
    if body.get("input").is_some() {
        let mut out = body.clone();
        out["model"] = json!(model);
        out["stream"] = json!(true);
        if out.get("max_output_tokens").is_none() {
            if let Some(comp) = out.get("max_completion_tokens").cloned() {
                out["max_output_tokens"] = comp;
            } else if let Some(mt) = out.get("max_tokens").cloned() {
                out["max_output_tokens"] = mt;
            }
        }
        if let Some(obj) = out.as_object_mut() {
            obj.remove("max_tokens");
            obj.remove("max_completion_tokens");
        }
        return out;
    }

    let mut result = json!({
        "model": model,
        "input": [],
        "stream": true,
        "store": false
    });

    let mut has_system = false;
    let messages = body.get("messages").and_then(|m| m.as_array()).cloned().unwrap_or_default();

    for msg in messages {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");

        if role == roles::SYSTEM || role == roles::DEVELOPER {
            if !has_system {
                let instructions = extract_instructions_text(&msg.get("content").cloned().unwrap_or(Json::Null));
                result["instructions"] = json!(instructions);
                has_system = true;
            }
            continue;
        }

        if role == roles::USER || role == roles::ASSISTANT {
            if role == roles::ASSISTANT {
                if let Some(reasoning_item) = build_reasoning_input_item(&msg) {
                    result["input"].as_array_mut().unwrap().push(reasoning_item);
                }
            }

            let content_type = if role == roles::USER { responses_items::INPUT_TEXT } else { responses_items::OUTPUT_TEXT };
            let content: Vec<Json> = match msg.get("content") {
                Some(Json::String(s)) => vec![json!({"type": content_type, "text": s})],
                Some(Json::Array(arr)) => arr
                    .iter()
                    .map(|c| {
                        if c.get("type").and_then(|v| v.as_str()) == Some(openai_blocks::TEXT) {
                            json!({"type": content_type, "text": c.get("text").and_then(|v| v.as_str()).unwrap_or("")})
                        } else if c.get("type").and_then(|v| v.as_str()) == Some(openai_blocks::IMAGE_URL) {
                            let url = c.get("image_url").and_then(|u| u.get("url").or(Some(u))).and_then(|v| v.as_str()).unwrap_or("");
                            let detail = c.get("image_url").and_then(|u| u.get("detail")).and_then(|v| v.as_str()).unwrap_or("auto");
                            json!({"type": responses_items::INPUT_IMAGE, "image_url": url, "detail": detail})
                        } else if c.get("type").and_then(|v| v.as_str()) == Some(responses_items::INPUT_IMAGE) {
                            c.clone()
                        } else {
                            let text = c.get("text").or_else(|| c.get("content")).and_then(|v| v.as_str()).unwrap_or("");
                            json!({"type": content_type, "text": text})
                        }
                    })
                    .collect(),
                _ => Vec::new(),
            };

            if !content.is_empty() {
                result["input"].as_array_mut().unwrap().push(json!({
                    "type": responses_items::MESSAGE,
                    "role": role,
                    "content": content
                }));
            }
        }

        if role == roles::ASSISTANT {
            if let Some(tool_calls) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                for tc in tool_calls {
                    let name = tc.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str()).unwrap_or("").trim();
                    if name.is_empty() {
                        continue;
                    }
                    let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    let args = tc.get("function").and_then(|f| f.get("arguments")).cloned().unwrap_or_else(|| json!({}));
                    result["input"].as_array_mut().unwrap().push(json!({
                        "type": responses_items::FUNCTION_CALL,
                        "call_id": clamp_responses_call_id(id),
                        "name": &name[..name.len().min(MAX_TOOL_NAME_LEN)],
                        "arguments": coerce_responses_arguments(&args)
                    }));
                }
            }
        }

        if role == roles::TOOL {
            let id = msg.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("");
            let content = msg.get("content").cloned().unwrap_or(Json::Null);
            result["input"].as_array_mut().unwrap().push(json!({
                "type": responses_items::FUNCTION_CALL_OUTPUT,
                "call_id": clamp_responses_call_id(id),
                "output": coerce_responses_output(&content)
            }));
        }
    }

    if !has_system {
        result["instructions"] = json!("");
    }

    if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
        let converted: Vec<Json> = tools
            .iter()
            .filter_map(|t| {
                if t.get("type").and_then(|v| v.as_str()) == Some(openai_blocks::FUNCTION) {
                    let name = t.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str())?.trim();
                    if name.is_empty() {
                        return None;
                    }
                    Some(json!({
                        "type": openai_blocks::FUNCTION,
                        "name": &name[..name.len().min(MAX_TOOL_NAME_LEN)],
                        "description": t.get("function").and_then(|f| f.get("description")).cloned().unwrap_or(json!("")),
                        "parameters": normalize_tool_parameters(t.get("function").and_then(|f| f.get("parameters"))),
                        "strict": t.get("function").and_then(|f| f.get("strict")).cloned().unwrap_or(Json::Null)
                    }))
                } else {
                    Some(t.clone())
                }
            })
            .collect();
        result["tools"] = Json::Array(converted);
    }

    if let Some(temp) = body.get("temperature") {
        result["temperature"] = temp.clone();
    }
    if let Some(max) = body.get("max_output_tokens").or_else(|| body.get("max_completion_tokens")).or_else(|| body.get("max_tokens")) {
        result["max_output_tokens"] = max.clone();
    }
    if let Some(top_p) = body.get("top_p") {
        result["top_p"] = top_p.clone();
    }
    if let Some(r) = body.get("reasoning") {
        result["reasoning"] = r.clone();
    } else if let Some(re) = body.get("reasoning_effort") {
        result["reasoning"] = json!({"effort": re, "summary": "auto"});
    }
    if let Some(st) = body.get("service_tier") {
        result["service_tier"] = st.clone();
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_responses_to_openai_request() {
        let body = json!({
            "instructions": "Be accurate.",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]},
                {"type": "function_call", "call_id": "c1", "name": "search", "arguments": "{\"q\":\"x\"}"},
                {"type": "function_call_output", "call_id": "c1", "output": "found it"}
            ]
        });
        let result = openai_responses_to_openai_request("gpt-5", body, false, None, &Default::default());
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "Be accurate.");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"][0]["text"], "hello");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["tool_calls"][0]["function"]["name"], "search");
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["content"], "found it");
    }

    #[test]
    fn test_openai_to_responses_request() {
        let body = json!({
            "messages": [
                {"role": "system", "content": "Instructions."},
                {"role": "user", "content": "hello"},
                {"role": "assistant", "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{}"}}]},
                {"role": "tool", "tool_call_id": "c1", "content": "res"}
            ]
        });
        let result = openai_to_openai_responses_request("gpt-5", body, true, None, &Default::default());
        assert_eq!(result["instructions"], "Instructions.");
        let input = result["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[2]["type"], "function_call_output");
    }
}
