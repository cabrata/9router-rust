use nr_core::Json;

/// Detect request format from body structure
pub fn detect_format(body: &Json) -> &'static str {
    // OpenAI Responses API
    if has_input_field(body) && !has_messages_field(body) {
        return "openai-responses";
    }

    // Antigravity format
    if is_antigravity(body) {
        return "antigravity";
    }

    // Gemini format
    if has_contents_array(body) {
        return "gemini";
    }

    // OpenAI-specific indicators
    if is_openai_specific(body) {
        return "openai";
    }

    // Claude format
    if is_claude_format(body) {
        return "claude";
    }

    "openai"
}

/// Resolve the API type for an openai-compatible node
pub fn resolve_openai_compatible_api_type(provider: &str, credentials: Option<&Json>) -> String {
    if let Some(creds) = credentials {
        if let Some(stored) = creds
            .get("providerSpecificData")
            .and_then(|v| v.get("apiType"))
            .and_then(|v| v.as_str())
        {
            if stored == "chat" || stored == "responses" {
                return stored.to_string();
            }
        }
    }

    if provider.contains("responses") {
        "responses".to_string()
    } else {
        "chat".to_string()
    }
}

/// Get target format for provider
pub fn get_target_format(provider: &str, credentials: Option<&Json>) -> &'static str {
    if provider.starts_with("openai-compatible-") {
        let api_type = resolve_openai_compatible_api_type(provider, credentials);
        if api_type == "responses" {
            "openai-responses"
        } else {
            "openai"
        }
    } else if provider.starts_with("anthropic-compatible-") {
        "claude"
    } else {
        match provider {
            "anthropic" => "claude",
            "gemini" | "gemini-cli" => "gemini",
            _ => "openai",
        }
    }
}

/// Check if last message is from user
pub fn is_last_message_from_user(body: &Json) -> bool {
    let messages = body
        .get("messages")
        .or_else(|| body.get("contents"))
        .and_then(|v| v.as_array());

    if let Some(msgs) = messages {
        if msgs.is_empty() {
            return true;
        }
        if let Some(last_msg) = msgs.last() {
            if let Some(role) = last_msg.get("role").and_then(|v| v.as_str()) {
                return role == "user";
            }
        }
    }

    true
}

/// Check if request has thinking config
pub fn has_thinking_config(body: &Json) -> bool {
    body.get("reasoning_effort").is_some() || has_thinking_enabled(body)
}

/// Normalize provider-native thinking config based on last message role
pub fn normalize_thinking_config(body: &mut Json) {
    if !is_last_message_from_user(body) {
        if let Some(obj) = body.as_object_mut() {
            if let Some(last_msg) = obj
                .get("messages")
                .and_then(|v| v.as_array())
                .and_then(|msgs| msgs.last())
            {
                if let Some(role) = last_msg.get("role").and_then(|v| v.as_str()) {
                    if role != "tool" {
                        obj.remove("thinking");
                    }
                }
            }
        }
    }
}

// === Helpers ===

fn has_input_field(body: &Json) -> bool {
    if let Some(input) = body.get("input") {
        matches!(input, serde_json::Value::Array(_) | serde_json::Value::String(_))
    } else {
        false
    }
}

fn has_messages_field(body: &Json) -> bool {
    body.get("messages").and_then(|v| v.as_array()).is_some()
}

fn is_antigravity(body: &Json) -> bool {
    body.get("request")
        .and_then(|r| r.get("contents"))
        .is_some()
        && body
            .get("userAgent")
            .and_then(|v| v.as_str())
            .map(|u| u == "antigravity")
            .unwrap_or(false)
}

fn has_contents_array(body: &Json) -> bool {
    body.get("contents").and_then(|v| v.as_array()).is_some()
}

fn is_openai_specific(body: &Json) -> bool {
    body.get("stream_options").is_some()
        || body.get("response_format").is_some()
        || body.get("logprobs").is_some()
        || body.get("top_logprobs").is_some()
        || body.get("n").is_some()
        || body.get("presence_penalty").is_some()
        || body.get("frequency_penalty").is_some()
        || body.get("logit_bias").is_some()
        || body.get("user").is_some()
}

fn is_claude_format(body: &Json) -> bool {
    if let Some(messages) = body.get("messages").and_then(|v| v.as_array()) {
        if let Some(first_msg) = messages.first() {
            if let Some(content) = first_msg.get("content") {
                if let Some(content_arr) = content.as_array() {
                    if let Some(first_content) = content_arr.first() {
                        if first_content
                            .get("type")
                            .and_then(|t| t.as_str())
                            .map(|t| t == "text")
                            .unwrap_or(false)
                        {
                            if body.get("system").is_some() || body.get("anthropic_version").is_some() {
                                return true;
                            }

                            if has_claude_image(content_arr) {
                                return true;
                            }

                            if has_claude_tools(content_arr) {
                                return true;
                            }
                        }
                    }
                }
            }
        }
    }

    body.get("system").is_some() || body.get("anthropic_version").is_some()
}

fn has_claude_image(content: &[Json]) -> bool {
    content.iter().any(|c| {
        c.get("type")
            .and_then(|t| t.as_str())
            .map(|t| t == "image")
            .unwrap_or(false)
            && c.get("source")
                .and_then(|s| s.get("type"))
                .and_then(|t| t.as_str())
                .map(|t| t == "base64")
                .unwrap_or(false)
    })
}

fn has_claude_tools(content: &[Json]) -> bool {
    content.iter().any(|c| {
        let t = c
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        t == "tool_use" || t == "tool_result"
    })
}

fn has_thinking_enabled(body: &Json) -> bool {
    body.get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(|t| t.as_str())
        .map(|t| t == "enabled")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_detect_format_openai_responses() {
        let body = json!({"input": ["test"], "model": "gpt-4"});
        assert_eq!(detect_format(&body), "openai-responses");
    }

    #[test]
    fn test_detect_format_openai() {
        let body = json!({"messages": [{"role": "user", "content": "test"}], "stream_options": {}, "model": "gpt-4"});
        assert_eq!(detect_format(&body), "openai");
    }

    #[test]
    fn test_detect_format_claude() {
        let body = json!({"messages": [{"role": "user", "content": [{"type": "text", "text": "hello"}]}], "anthropic_version": "2023-06-01", "model": "claude-3"});
        assert_eq!(detect_format(&body), "claude");
    }

    #[test]
    fn test_resolve_openai_compatible() {
        assert_eq!(resolve_openai_compatible_api_type("openai-compatible-chat-uuid", None), "chat");
        assert_eq!(resolve_openai_compatible_api_type("openai-compatible-responses-uuid", None), "responses");
    }

    #[test]
    fn test_is_last_message_from_user() {
        let body_user = json!({"messages": [{"role": "user", "content": "hello"}]});
        assert!(is_last_message_from_user(&body_user));

        let body_assistant = json!({"messages": [{"role": "assistant", "content": "hi"}]});
        assert!(!is_last_message_from_user(&body_assistant));
    }
}
