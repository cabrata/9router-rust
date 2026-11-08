//! Port of open-sse/executors/cursor.js
//! Cursor agent executor with protobuf streaming and checksum validation.
//!
//! Wave 1: Minimal implementation that compiles and delegates to base_execute.
//! Still needed: Protobuf encode/decode, checksum validation, bidirectional streaming,
//! tool calling, kv metadata handling.

use async_trait::async_trait;
use nr_core::http::HeaderList;
use nr_core::{Credentials, ExecRequest, ExecResult, Json, Result};
use nr_exec_base::BaseHooks;
use std::sync::Arc;

pub struct CursorExecutor {
    provider: String,
    config: Json,
}

impl CursorExecutor {
    pub fn new(provider_id: &str) -> Self {
        let config = nr_data::provider(provider_id).cloned().unwrap_or_default();
        Self {
            provider: provider_id.to_string(),
            config,
        }
    }
}

#[async_trait]
impl BaseHooks for CursorExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn config(&self) -> &Json {
        &self.config
    }

    fn build_headers(
        &self,
        creds: &Credentials,
        _stream: bool,
        _url: &str,
        _model: &str,
        _body: &Json,
    ) -> Result<HeaderList> {
        let mut headers = HeaderList::new();
        headers.insert("Content-Type".into(), "application/json".into());

        // TODO(dedupe): cursor auth - API key or token-based auth
        if let Some(api_key) = creds.api_key() {
            headers.insert("Authorization".into(), format!("Bearer {}", api_key));
        }

        Ok(headers)
    }
}

#[async_trait]
impl nr_core::Executor for CursorExecutor {
    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        // shortcut: This delegates to base_execute without Cursor-specific features.
        // Full implementation requires:
        // - Protobuf encoding/decoding via cursorProtobuf.js logic
        // - Checksum validation via cursorChecksum.js
        // - HTTP/2 bidirectional streaming for tool calls
        // - KV metadata handling for long-running agent sessions
        // - Response format setting for protobuf responses
        nr_exec_base::base_execute(self, req).await
    }
}

pub fn create(provider: &str) -> Arc<dyn nr_core::Executor> {
    Arc::new(CursorExecutor::new(provider))
}
