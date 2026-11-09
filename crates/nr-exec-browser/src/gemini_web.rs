//! Port of open-sse/executors/gemini-web.js
//! Type browser automation of Gemini web UI
//! Auth: Cookie-based (__Secure-1PSID + __Secure-1PSIDTS from gemini.google.com)
//! Method: Playwright browser automation

use nr_core::Executor;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

const GEMINI_URL: &str = "https://gemini.google.com/app";
const DEFAULT_MODEL_ID: &str = "gemini-2-5-pro";
const GEMINI_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36";

/// GeminiWebExecutor — Type-based browser automation of Gemini web UI
pub struct GeminiWebExecutor;

#[async_trait]
impl Executor for GeminiWebExecutor {
    async fn execute(
        &self,
        req: &nr_core::ExecutorRequest,
    ) -> Result<nr_core::ExecutorResponse, Box<dyn std::error::Error>> {
        // TODO: Implement full Gemini web executor
        // This requires:
        // 1. Browser context creation with cookies
        // 2. Model selection via UI toggle
        // 3. Prompt typing and submission
        // 4. DOM polling for response
        // 5. SSE streaming or buffered response

        // For now, return error indicating web automation not yet implemented
        let error_response = json!({
            "error": {
                "message": "gemini-web browser automation not yet implemented in Rust port",
                "type": "internal_error"
            }
        });

        Ok(nr_core::ExecutorResponse {
            status: 503,
            headers: std::collections::HashMap::from([
                ("content-type".to_string(), "application/json".to_string()),
                ("retry-after".to_string(), "60".to_string()),
            ]),
            body: error_response,
            stream: None,
        })
    }
}

/// Create executor for the canonical provider id
pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(GeminiWebExecutor)
}

/// Build Gemini prompt from OpenAI messages (flatten multi-turn to single prompt)
pub fn build_gemini_prompt(messages: &[Value]) -> String {
    let text_messages: Vec<_> = messages
        .iter()
        .filter(|m| {
            m.get("content")
                .and_then(|c| c.as_str())
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
        })
        .collect();

    let user_messages: Vec<_> = text_messages
        .iter()
        .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        .collect();

    let last_user_msg = user_messages.last();
    let last_user_idx = last_user_msg
        .and_then(|m| {
            text_messages
                .iter()
                .position(|msg| msg.get("content") == m.get("content"))
        })
        .unwrap_or(0);

    let last_user_content = last_user_msg
        .and_then(|m| m.get("content").and_then(|c| c.as_str()))
        .unwrap_or("")
        .to_string();

    // Prior conversation = every user/assistant turn before the final user turn
    let prior_turns: Vec<_> = text_messages
        .iter()
        .enumerate()
        .filter(|(i, m)| {
            *i < last_user_idx
                && ["user", "assistant"]
                    .contains(&m.get("role").and_then(|r| r.as_str()).unwrap_or(""))
        })
        .map(|(_, m)| m)
        .collect();

    let system_text = text_messages
        .iter()
        .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("system"))
        .filter_map(|m| m.get("content").and_then(|c| c.as_str()))
        .collect::<Vec<_>>()
        .join("\n\n");

    // Single-turn (no earlier user/assistant turns) with no system message:
    // return only the last user message
    if prior_turns.is_empty() && system_text.is_empty() {
        return last_user_content;
    }

    // Single-turn with a system message: prepend it
    if prior_turns.is_empty() {
        return format!("System:\n{}\n\n{}", system_text, last_user_content);
    }

    // Multi-turn: include prior conversation history
    let history = prior_turns
        .iter()
        .filter_map(|m| {
            let role = m.get("role").and_then(|r| r.as_str())?;
            let content = m.get("content").and_then(|c| c.as_str())?;
            let role_name = if role == "assistant" {
                "Assistant"
            } else {
                "User"
            };
            Some(format!("{}: {}", role_name, content))
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    let mut parts = vec![];
    if !system_text.is_empty() {
        parts.push(format!("System:\n{}", system_text));
    }
    parts.push(format!("Previous conversation:\n{}", history));
    parts.push(format!("Current user message:\n{}", last_user_content));

    parts.join("\n\n")
}

/// Format response as OpenAI chat completion
fn format_chat_completion(content: &str, model: &str) -> Value {
    json!({
        "id": format!("chatcmpl-{}", chrono::Local::now().timestamp_millis()),
        "object": "chat.completion",
        "created": chrono::Local::now().timestamp(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": content
            },
            "finish_reason": "stop",
            "logprobs": null
        }],
        "usage": {
            "prompt_tokens": 0,
            "completion_tokens": 0,
            "total_tokens": 0
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_gemini_prompt_single_user() {
        let messages = json!([
            {"role": "user", "content": "Hello"}
        ]);

        let prompt = build_gemini_prompt(&[messages[0].clone()]);
        assert_eq!(prompt, "Hello");
    }

    #[test]
    fn test_build_gemini_prompt_with_system() {
        let messages = vec![
            json!({"role": "system", "content": "Be helpful"}),
            json!({"role": "user", "content": "Hello"}),
        ];

        let prompt = build_gemini_prompt(&messages);
        assert!(prompt.contains("System:"));
        assert!(prompt.contains("Be helpful"));
        assert!(prompt.contains("Hello"));
    }

    #[test]
    fn test_format_chat_completion() {
        let response = format_chat_completion("Test response", "gemini-2-5-pro");
        assert_eq!(response["model"], "gemini-2-5-pro");
        assert_eq!(response["choices"][0]["message"]["content"], "Test response");
    }
}
