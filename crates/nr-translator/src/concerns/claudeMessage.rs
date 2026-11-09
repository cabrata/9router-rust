//! Convert OpenAI completion responses to Claude message format. Port of open-sse/translator/concerns/claudeMessage.js.

use nr_core::Json;
use serde_json::json;
use crate::schema::{claude_blocks, responses_items, roles};
use crate::concerns::json::safe_parse_json;

fn parse_tool_arguments(value: &Json) -> Json {
    if let Some(obj) = value.as_object() {
        Json::Object(obj.clone())
    } else if let Some(s) = value.as_str() {
        safe_parse_json(s, json!({}))
    } else {
        json!({})
    }
}

/// Convert OpenAI completion response (non-streaming) to Claude message format.
pub fn openai_completion_to_claude_message(response_body: &Json) -> Json {
    if response_body.get("choices").and_then(|v| v.as_array()).and_then(|a| a.first()).is_none() {
        return response_body.clone();
    }

    let choice = &response_body["choices"][0];
    let message = &choice["message"];

    let mut content = Vec::new();

    // Extract reasoning
    let reasoning = message
        .get("reasoning_content")
        .or_else(|| message.get("provider_specific_fields").and_then(|v| v.get("reasoning_content")))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if !reasoning.is_empty() {
        content.push(json!({
            "type": claude_blocks::THINKING,
            "thinking": reasoning
        }));
    }

    // Extract text
    if let Some(text) = message.get("content").and_then(|v| v.as_str()) {
        if !text.is_empty() {
            content.push(json!({
                "type": claude_blocks::TEXT,
                "text": text
            }));
        }
    }

    // Extract tool calls
    if let Some(tool_calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
        let empty = Json::Object(serde_json::Map::new());
        for (idx, tool_call) in tool_calls.iter().enumerate() {
            let fn_obj = tool_call.get("function").unwrap_or(&empty);
            let fn_name = fn_obj.get("name").or_else(|| tool_call.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let fn_args = fn_obj.get("arguments").or_else(|| tool_call.get("arguments"))
                .unwrap_or(&empty);
            let tool_id = tool_call.get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis();
                    format!("toolu_{}_{}", now, idx)
                });

            content.push(json!({
                "type": claude_blocks::TOOL_USE,
                "id": tool_id,
                "name": fn_name,
                "input": parse_tool_arguments(fn_args),
            }));
        }
    }

    if content.is_empty() {
        content.push(json!({
            "type": claude_blocks::TEXT,
            "text": ""
        }));
    }

    let usage = &response_body["usage"];
    let msg_id = response_body.get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.replace("chatcmpl-", ""))
        .unwrap_or_else(|| {
            format!("msg_{}", std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis())
        });

    json!({
        "id": msg_id,
        "type": responses_items::MESSAGE,
        "role": roles::ASSISTANT,
        "model": response_body.get("model").and_then(|v| v.as_str()).unwrap_or("unknown"),
        "content": content,
        "stop_reason": crate::concerns::finish_reason::openai_to_claude_finish_reason(
            choice.get("finish_reason").and_then(|v| v.as_str()).unwrap_or("")
        ),
        "stop_sequence": serde_json::Value::Null,
        "usage": {
            "input_tokens": usage.get("prompt_tokens").or_else(|| usage.get("input_tokens")).and_then(|v| v.as_i64()).unwrap_or(0),
            "output_tokens": usage.get("completion_tokens").or_else(|| usage.get("output_tokens")).and_then(|v| v.as_i64()).unwrap_or(0),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_openai_completion_to_claude_message_with_text() {
        let response = json!({
            "id": "chatcmpl-123",
            "model": "gpt-4",
            "choices": [{
                "message": {
                    "content": "Hello"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5
            }
        });

        let claude_msg = openai_completion_to_claude_message(&response);
        assert_eq!(claude_msg.get("role"), Some(&json!(roles::ASSISTANT)));
        assert!(claude_msg.get("content").and_then(|v| v.as_array()).is_some());
    }
}
