//! Port of open-sse/executors/chatgpt-web.js — ChatGPT web chat executor.
//!
//! Handles browser session-based auth, DALL-E image generation, and tool calling.
//! TODO(dedupe): Implement full browser session, deltaV1 streaming, tool translation.

use std::sync::Arc;
use async_trait::async_trait;
use nr_core::{Credentials, ExecRequest, ExecResult, Executor, Result};

/// ChatGPT web executor — handles chatgpt.com web API (browser session auth)
pub struct ChatGptWebExecutor;

#[async_trait]
impl Executor for ChatGptWebExecutor {
    fn provider(&self) -> &str {
        "chatgpt-web"
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        // TODO: Implement full execution path:
        // 1. Browser session management (accessToken from cookies/localStorage)
        // 2. First-party endpoints for message creation
        // 3. deltaV1 SSE streaming format
        // 4. Image generation via DALL-E integration
        // 5. Tool calling support

        // For now, delegate to default executor
        let default_exec = nr_exec_base::default_executor::create(self.provider());
        default_exec.execute(req).await
    }
}

/// Create the executor instance
pub fn create(provider: &str) -> Arc<dyn Executor> {
    Arc::new(ChatGptWebExecutor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_name() {
        let exec = ChatGptWebExecutor;
        assert_eq!(exec.provider(), "chatgpt-web");
    }
}
