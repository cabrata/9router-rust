//! Port of open-sse/executors/uc-web.js — UC (UcChat) web executor.
//!
//! Handles web-based chat via UC Chat API.
//! TODO(dedupe): Implement full UC Chat session and streaming.

use std::sync::Arc;
use async_trait::async_trait;
use nr_core::{Credentials, ExecRequest, ExecResult, Executor, Result};

/// UC web executor — handles ucchat.com web API
pub struct UcWebExecutor;

#[async_trait]
impl Executor for UcWebExecutor {
    fn provider(&self) -> &str {
        "uc-web"
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        // TODO: Implement full execution path:
        // 1. Session-based auth
        // 2. UC Chat streaming format

        // For now, delegate to default executor
        let default_exec = nr_exec_base::default_executor::create(self.provider());
        default_exec.execute(req).await
    }
}

/// Create the executor instance
pub fn create(provider: &str) -> Arc<dyn Executor> {
    Arc::new(UcWebExecutor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_name() {
        let exec = UcWebExecutor;
        assert_eq!(exec.provider(), "uc-web");
    }
}
