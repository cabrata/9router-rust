//! Port of open-sse/executors/perplexity-web.js — Perplexity web chat executor.
//!
//! Handles web search API with streaming response.
//! TODO(dedupe): Implement full web search session and streaming format.

use std::sync::Arc;
use async_trait::async_trait;
use nr_core::{Credentials, ExecRequest, ExecResult, Executor, Result};

/// Perplexity web executor — handles api.perplexity.ai web interface
pub struct PerplexityWebExecutor;

#[async_trait]
impl Executor for PerplexityWebExecutor {
    fn provider(&self) -> &str {
        "perplexity-web"
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        // TODO: Implement full execution path:
        // 1. Session-based auth
        // 2. Web search integration
        // 3. Citation formatting

        // For now, delegate to default executor
        let default_exec = nr_exec_base::default_executor::create(self.provider());
        default_exec.execute(req).await
    }
}

/// Create the executor instance
pub fn create(provider: &str) -> Arc<dyn Executor> {
    Arc::new(PerplexityWebExecutor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_name() {
        let exec = PerplexityWebExecutor;
        assert_eq!(exec.provider(), "perplexity-web");
    }
}
