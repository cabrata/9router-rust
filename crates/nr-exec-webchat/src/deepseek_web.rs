//! Port of open-sse/executors/deepseek-web.js — DeepSeek web chat executor.
//!
//! Handles Bearer token auth, PoW challenges, session management, and SSE streaming.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use async_trait::async_trait;
use tokio::sync::Mutex;
use serde_json::{json, Value as Json};
use nr_core::{Credentials, ExecRequest, ExecResult, Executor, Result, UpstreamResponse};
use crate::deepseek_utils::{
    DeepSeekPowChallenge, solve_deepseek_pow, is_thinking_model, is_search_model,
    format_stream_content, append_search_citations,
};
use crate::web_tools::{serialize_deepseek_tool_prompt, parse_deepseek_tool_calls};

const DEEPSEEK_WEB_BASE: &str = "https://chat.deepseek.com";
const DEEPSEEK_API_BASE: &str = "https://chat.deepseek.com/api";
const COMPLETION_URL: &str = "https://chat.deepseek.com/api/v0/chat/completion";

/// Cache for access tokens (userToken → accessToken + expiry).
static TOKEN_CACHE: std::sync::LazyLock<Mutex<std::collections::HashMap<String, (String, u64)>>> = std::sync::LazyLock::new(Default::default);
static SESSION_CACHE: std::sync::LazyLock<Mutex<std::collections::HashMap<String, (String, u64)>>> = std::sync::LazyLock::new(Default::default);
const CACHE_MAX_SIZE: usize = 100;

/// DeepSeek web executor — handles chat.deepseek.com web API
pub struct DeepSeekWebExecutor;

#[async_trait]
impl Executor for DeepSeekWebExecutor {
    fn provider(&self) -> &str {
        "deepseek-web"
    }

    async fn execute(&self, _req: ExecRequest<'_>) -> Result<ExecResult> {
        // Extract user token from credentials
        let _user_token = extract_user_token(_req.credentials)
            .ok_or_else(|| nr_core::Error::other("Missing userToken for DeepSeek Web"))?;

        // TODO: Implement full execution:
        // 1. Acquire access token via /users/current
        // 2. Create or reuse session via /chat_session/create
        // 3. Request PoW challenge via /chat/create_pow_challenge
        // 4. Solve PoW with solve_deepseek_pow()
        // 5. POST to /chat/completion with PoW header
        // 6. Parse DeepSeek SSE stream and transform to OpenAI format
        // 7. Handle tool translation if tools[] present

        // For now, use default executor stub
        let default_exec = nr_exec_base::default_executor::create(self.provider());
        default_exec.execute(_req).await
    }
}

/// Extract user token from credentials (raw or JSON-wrapped).
fn extract_user_token(creds: &Credentials) -> Option<String> {
    let raw = creds.api_key().or_else(|| creds.access_token())?;

    // Try to parse as JSON first (DeepSeek may store as `{"value":"..."}`)
    if let Ok(parsed) = serde_json::from_str::<Json>(raw) {
        if let Some(value) = parsed.get("value").and_then(|v| v.as_str()) {
            return Some(value.to_string());
        }
    }

    // Otherwise use raw token
    if !raw.is_empty() {
        Some(raw.to_string())
    } else {
        None
    }
}

/// Build a prompt from messages array (single prompt string for web API).
fn messages_to_prompt(messages: &[Json], history_window: usize) -> String {
    let mut system_parts = vec![];
    let mut conversation = vec![];
    let mut call_name_by_id: std::collections::HashMap<String, String> = Default::default();
    let mut last_user_content = String::new();

    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let text = extract_message_text(msg.get("content").unwrap_or(&Json::Null));

        match role {
            "system" if !text.is_empty() => system_parts.push(text),
            "user" | "assistant" if !text.is_empty() => {
                conversation.push((role.to_string(), text.clone()));
                if role == "user" {
                    last_user_content = text;
                }
                // Track tool calls
                if let Some(calls) = msg.get("tool_calls").and_then(|tc| tc.as_array()) {
                    for call in calls {
                        if let (Some(id), Some(fn_obj)) = (
                            call.get("id").and_then(|i| i.as_str()),
                            call.get("function").and_then(|f| f.as_object()),
                        ) {
                            if let Some(name) = fn_obj.get("name").and_then(|n| n.as_str()) {
                                call_name_by_id.insert(id.to_string(), name.to_string());
                            }
                        }
                    }
                }
            }
            "tool" if !text.is_empty() => {
                let name = msg
                    .get("tool_call_id")
                    .and_then(|id| call_name_by_id.get(id.as_str().unwrap_or("")))
                    .or_else(|| msg.get("name").and_then(|n| n.as_str()))
                    .unwrap_or("tool");
                conversation.push(("tool".to_string(), format!("({}) {}", name, text)));
            }
            _ => {}
        }
    }

    let mut parts = vec![];
    if !system_parts.is_empty() {
        parts.push(system_parts.join("\n\n"));
    }

    let effective_window = if history_window > 0 {
        history_window
    } else if conversation.len() > 1 {
        20 // DEFAULT_AUTO_HISTORY_WINDOW
    } else {
        0
    };

    if effective_window > 0 && conversation.len() > 1 {
        let recent = &conversation[conversation.len().saturating_sub(effective_window)..];
        let transcript = recent
            .iter()
            .map(|(role, text)| {
                match role.as_str() {
                    "assistant" => format!("Assistant: {}", text),
                    "tool" => format!("Tool result {}", text),
                    _ => format!("User: {}", text),
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        parts.push(transcript);
    } else if !last_user_content.is_empty() {
        parts.push(last_user_content);
    }

    // Strip image markdown
    parts.join("\n\n").replace("![", "[").replace("]", "")
}

/// Extract text from message content.
fn extract_message_text(content: &Json) -> String {
    if let Some(arr) = content.as_array() {
        arr.iter()
            .filter(|item| item.get("type").map(|t| t.as_str()) == Some(Some("text")))
            .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        content.as_str().unwrap_or("").to_string()
    }
}

/// Create the executor instance
pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(DeepSeekWebExecutor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_user_token_raw() {
        let mut creds = Credentials::new();
        creds.set("apiKey", "raw-token-string");
        assert_eq!(extract_user_token(&creds), Some("raw-token-string".to_string()));
    }

    #[test]
    fn test_extract_user_token_json_wrapped() {
        let mut creds = Credentials::new();
        creds.set("apiKey", r#"{"value":"json-wrapped-token"}"#);
        assert_eq!(extract_user_token(&creds), Some("json-wrapped-token".to_string()));
    }

    #[test]
    fn test_extract_user_token_with_access_token() {
        let mut creds = Credentials::new();
        creds.set("accessToken", "access-token-value");
        assert_eq!(extract_user_token(&creds), Some("access-token-value".to_string()));
    }

    #[test]
    fn test_extract_user_token_empty() {
        let creds = Credentials::new();
        assert_eq!(extract_user_token(&creds), None);
    }

    #[test]
    fn test_extract_user_token_empty_string() {
        let mut creds = Credentials::new();
        creds.set("apiKey", "");
        assert_eq!(extract_user_token(&creds), None);
    }

    #[test]
    fn test_messages_to_prompt_single_user() {
        let messages = vec![json!({"role": "user", "content": "Hello"})];
        let prompt = messages_to_prompt(&messages, 0);
        assert!(prompt.contains("Hello"));
    }

    #[test]
    fn test_messages_to_prompt_with_system() {
        let messages = vec![
            json!({"role": "system", "content": "System message"}),
            json!({"role": "user", "content": "User message"}),
        ];
        let prompt = messages_to_prompt(&messages, 0);
        assert!(prompt.contains("System message"));
        assert!(prompt.contains("User message"));
    }

    #[test]
    fn test_provider_name() {
        let exec = DeepSeekWebExecutor;
        assert_eq!(exec.provider(), "deepseek-web");
    }
}
