//! Port of open-sse/executors/deepseek-web.js — DeepSeek web chat executor.
//!
//! Handles Bearer token auth, PoW challenges, session management, and SSE streaming.
//! TODO(dedupe): Implement full PoW solver, session persistence, tool translation.

use std::sync::Arc;
use async_trait::async_trait;
use nr_core::http::{proxy_fetch, FetchRequest, HeaderList};
use nr_core::{Credentials, ExecRequest, ExecResult, Executor, Result};

const DEEPSEEK_WEB_BASE: &str = "https://chat.deepseek.com";
const COMPLETION_URL: &str = "https://chat.deepseek.com/api/v0/chat/completion";

/// DeepSeek web executor — handles chat.deepseek.com web API (Bearer token auth, PoW-gated)
pub struct DeepSeekWebExecutor;

#[async_trait]
impl Executor for DeepSeekWebExecutor {
    fn provider(&self) -> &str {
        "deepseek-web"
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        // Extract user token from credentials (raw or JSON-wrapped)
        let user_token = extract_user_token(req.credentials)
            .ok_or_else(|| nr_core::Error::auth("Missing userToken for DeepSeek Web"))?;

        // TODO: Implement full execution path:
        // 1. Acquire access token from /users/current
        // 2. Request PoW challenge from /chat/create_pow_challenge
        // 3. Solve PoW challenge
        // 4. Make completion request with PoW header
        // 5. Transform DeepSeek SSE to OpenAI format
        // 6. Handle tool translation if tools[] present

        // For now, delegate to default executor which at least handles the request
        let default_exec = nr_exec_base::default_executor::create(self.provider());
        default_exec.execute(req).await
    }
}

/// Extract user token from credentials (raw or JSON-wrapped).
/// DeepSeek may store token as `{"value":"..."}` or raw string.
fn extract_user_token(creds: &Credentials) -> Option<String> {
    let raw = creds.api_key().or_else(|| creds.access_token())?;

    // Try to parse as JSON first
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(raw) {
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

/// Create the executor instance
pub fn create(provider: &str) -> Arc<dyn Executor> {
    Arc::new(DeepSeekWebExecutor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_user_token_raw() {
        let mut creds = Credentials::new();
        creds.set_api_key("raw-token-string");
        assert_eq!(extract_user_token(&creds), Some("raw-token-string".to_string()));
    }

    #[test]
    fn test_extract_user_token_json_wrapped() {
        let mut creds = Credentials::new();
        creds.set_api_key(r#"{"value":"json-wrapped-token"}"#);
        assert_eq!(extract_user_token(&creds), Some("json-wrapped-token".to_string()));
    }

    #[test]
    fn test_extract_user_token_empty() {
        let creds = Credentials::new();
        assert_eq!(extract_user_token(&creds), None);
    }

    #[test]
    fn test_provider_name() {
        let exec = DeepSeekWebExecutor;
        assert_eq!(exec.provider(), "deepseek-web");
    }
}
