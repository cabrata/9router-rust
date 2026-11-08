//! Port of open-sse/executors/windsurf.js.
//!
//! WindsurfExecutor routes requests via Codeium's gRPC-web chat service.
//!
//! Wire protocol: gRPC-web over HTTPS (Content-Type: application/grpc-web+proto).
//! Service:  exa.language_server_pb.LanguageServerService
//! Method:   GetChatMessage (unary request → streamed CompletionChunk frames)
//!
//! TODO(dedupe): Complete port requires:
//! - gRPC-web wire format support (protobuf encoding/decoding)
//! - Model alias mapping (100+ effort-level variants)
//! - Metadata/request frame construction
//! - Stream response unwrapping
//!
//! shortcut: Currently delegates to DefaultExecutor. Full gRPC-web support
//! would require substantial infrastructure (protobuf, wire format handling).
//! Upgrade when gRPC-web stack is available or if Windsurf becomes priority.

use std::sync::Arc;

use nr_core::{ExecRequest, ExecResult, Executor, Result};

pub struct WindsurfExecutor {
    provider: String,
    config: &'static nr_core::Json,
}

impl WindsurfExecutor {
    pub fn new(provider: &str) -> Self {
        let config = nr_data::provider(provider)
            .or_else(|| nr_data::provider("windsurf"))
            .unwrap_or_else(|| &nr_core::Json::Object(Default::default()));
        Self {
            provider: provider.to_string(),
            config,
        }
    }
}

impl nr_exec_base::base::BaseHooks for WindsurfExecutor {
    fn provider(&self) -> &str {
        "windsurf"
    }

    fn config(&self) -> &nr_core::Json {
        self.config
    }
}

#[async_trait::async_trait]
impl Executor for WindsurfExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn no_auth(&self) -> bool {
        false
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        // TODO(dedupe): Implement gRPC-web chat protocol.
        // For now, delegate to default executor.
        nr_exec_base::default_executor::create("windsurf").execute(req).await
    }
}

pub fn create(provider: &str) -> Arc<dyn Executor> {
    Arc::new(WindsurfExecutor::new(provider))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windsurf_executor_creates() {
        let executor = WindsurfExecutor::new("windsurf");
        assert_eq!(executor.provider(), "windsurf");
    }
}
