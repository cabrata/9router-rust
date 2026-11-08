use nr_core::Json;
use serde_json::json;

const PLACEHOLDER: &str = " ";

const DEEPSEEK_V4_PRO: &str = "deepseek-v4-pro";

const MODEL_RULES: &[(&str, &str)] = &[
    ("kimi-", "toolCalls"),
    ("deepseek", "all"),
];

#[derive(Clone, Debug)]
pub struct ReasoningInjectionConfig {
    pub provider: String,
    pub model: String,
    pub body: Json,
}

#[derive(Clone, Debug)]
pub struct ReasoningInjectionResult {
    pub body: Json,
}

/// Check if message should have reasoning_content injected
fn should_inject(message: &Json, scope: &str) -> bool {
    if message.get("role").and_then(|v| v.as_str()) != Some("assistant") {
        return false;
    }

    if let Some(rc) = message.get("reasoning_content").and_then(|v| v.as_str()) {
        if !rc.is_empty() {
            return false;
        }
    }

    if scope == "toolCalls" {
        return message.get("tool_calls")
            .and_then(|tc| tc.as_array())
            .map(|arr| !arr.is_empty())
            .unwrap_or(false);
    }

    true
}

/// Apply injection rule to messages
fn apply_rule(body: &Json, scope: &str) -> Json {
    let messages = match body.get("messages").and_then(|v| v.as_array()) {
        Some(msgs) => msgs,
        None => return body.clone(),
    };

    let new_messages: Vec<Json> = messages.iter().map(|msg| {
        if should_inject(msg, scope) {
            let mut new_msg = msg.clone();
            if let Some(obj) = new_msg.as_object_mut() {
                obj.insert("reasoning_content".to_string(), json!(PLACEHOLDER));
            }
            new_msg
        } else {
            msg.clone()
        }
    }).collect();

    let mut result = body.clone();
    if let Some(obj) = result.as_object_mut() {
        obj.insert("messages".to_string(), json!(new_messages));
    }
    result
}

/// Apply DeepSeek V4 Pro alias transformations
fn apply_deepseek_v4_pro_alias(provider: &str, model: &str, body: &Json) -> Json {
    if provider != "deepseek" {
        return body.clone();
    }

    let alias_config = match model {
        "deepseek-v4-pro-max" => Some((Some("enabled"), Some("max"))),
        "deepseek-v4-pro-none" => Some((Some("disabled"), None)),
        _ => None,
    };

    match alias_config {
        None => body.clone(),
        Some((thinking_type, reasoning_effort)) => {
            let mut result = body.clone();
            if let Some(obj) = result.as_object_mut() {
                obj.insert("model".to_string(), json!(DEEPSEEK_V4_PRO));

                let mut extra_body = obj.get("extra_body")
                    .and_then(|v| v.as_object())
                    .cloned()
                    .unwrap_or_default();

                let mut thinking = extra_body.get("thinking")
                    .and_then(|v| v.as_object())
                    .cloned()
                    .unwrap_or_default();

                if let Some(tt) = thinking_type {
                    thinking.insert("type".to_string(), json!(tt));
                }
                extra_body.insert("thinking".to_string(), json!(thinking));
                obj.insert("extra_body".to_string(), json!(extra_body));

                if let Some(re) = reasoning_effort {
                    obj.insert("reasoning_effort".to_string(), json!(re));
                } else {
                    obj.remove("reasoning_effort");
                }
            }
            result
        }
    }
}

/// Inject reasoning content based on provider and model configuration
/// Parameter order: body, provider, model
pub fn inject_reasoning_content(body: &Json, provider: &str, model: &str) -> Json {
    // First apply any DeepSeek V4 Pro alias transformations
    let body_after_alias = apply_deepseek_v4_pro_alias(provider, model, body);

    // Determine scope: check model rules first, then provider rules
    let scope = {
        let mut found = None;
        for (pattern, rule_scope) in MODEL_RULES {
            if model.to_lowercase().contains(pattern) {
                found = Some(*rule_scope);
                break;
            }
        }
        found.unwrap_or("all")
    };

    // Apply the injection rule
    apply_rule(&body_after_alias, scope)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_inject_for_empty_reasoning() {
        let msg = json!({
            "role": "assistant",
            "content": "hello",
            "reasoning_content": ""
        });
        assert!(should_inject(&msg, "all"));
    }

    #[test]
    fn test_should_not_inject_for_present_reasoning() {
        let msg = json!({
            "role": "assistant",
            "content": "hello",
            "reasoning_content": "some reasoning"
        });
        assert!(!should_inject(&msg, "all"));
    }

    #[test]
    fn test_should_not_inject_for_non_assistant() {
        let msg = json!({
            "role": "user",
            "content": "hello"
        });
        assert!(!should_inject(&msg, "all"));
    }

    #[test]
    fn test_inject_reasoning_content() {
        let body = json!({
            "messages": [
                {
                    "role": "assistant",
                    "content": "hello",
                    "reasoning_content": ""
                }
            ]
        });

        let result = inject_reasoning_content(&body, "deepseek", "deepseek-3.5");
        let reasoning = result
            .get("messages")
            .and_then(|msgs| msgs.get(0))
            .and_then(|msg| msg.get("reasoning_content"))
            .and_then(|rc| rc.as_str());

        assert_eq!(reasoning, Some(" "));
    }

    #[test]
    fn test_deepseek_v4_pro_alias_max() {
        let body = json!({
            "messages": [{"role": "assistant", "content": "hi"}],
            "model": "deepseek-v4-pro-max"
        });

        let result = apply_deepseek_v4_pro_alias("deepseek", "deepseek-v4-pro-max", &body);
        assert_eq!(result.get("model").and_then(|m| m.as_str()), Some(DEEPSEEK_V4_PRO));
        assert_eq!(result.get("reasoning_effort").and_then(|re| re.as_str()), Some("max"));
    }
}
