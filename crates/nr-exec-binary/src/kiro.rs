//! Port of open-sse/executors/kiro.js
//! Kiro (AWS CodeWhisperer) executor with EventStream support and integrity gating.
//!
//! Wave 1: Minimal implementation that compiles and delegates to base_execute.
//! Still needed: EventStream decoding, integrity gate, session replay, token refresh, repair logic.

use async_trait::async_trait;
use nr_core::http::HeaderList;
use nr_core::{Credentials, ExecRequest, ExecResult, Json, Result};
use nr_exec_base::BaseHooks;
use std::sync::Arc;
use uuid::Uuid;

pub struct KiroExecutor {
    provider: String,
    config: Json,
}

impl KiroExecutor {
    pub fn new(provider_id: &str) -> Self {
        let config = nr_data::provider(provider_id).cloned().unwrap_or_default();
        Self {
            provider: provider_id.to_string(),
            config,
        }
    }
}

#[async_trait]
impl BaseHooks for KiroExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn config(&self) -> &Json {
        &self.config
    }

    fn build_url(
        &self,
        _model: &str,
        _stream: bool,
        url_index: usize,
        _creds: &Credentials,
    ) -> Result<String> {
        // TODO(dedupe): getOrderedBaseUrls - auth-aware endpoint ordering
        // Kiro has multiple base URLs with fallback logic based on auth method (api_key, external_idp, etc.)
        let base_urls = self.base_urls();
        Ok(base_urls
            .get(url_index)
            .or_else(|| base_urls.first())
            .cloned()
            .unwrap_or_else(|| "https://runtime.us-east-1.kiro.dev".to_string()))
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
        headers.insert("Amz-Sdk-Request".into(), "attempt=1; max=3".into());
        headers.insert("Amz-Sdk-Invocation-Id".into(), Uuid::new_v4().to_string());

        // TODO(dedupe): buildHeaders - auth method handling (api_key vs OAuth tokens)
        if let Some(token) = creds.access_token() {
            headers.insert("Authorization".into(), format!("Bearer {}", token));
        }

        Ok(headers)
    }
}

#[async_trait]
impl nr_core::Executor for KiroExecutor {
    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        // shortcut: This delegates to base_execute without Kiro-specific features.
        // Full implementation requires:
        // - EventStream frame decoding via crate::aws_event_stream::parse_event_frame
        // - Integrity gate for tool_call repair (readRecoverableIntegrityAttempt, runIntegrityRecovery)
        // - Session replay via kiroSessionReplay.js logic
        // - Token refresh via nr_oauth::refresh_kiro_token
        // - Complex repair logic with regex-based heuristics for ellipsis/short_final/invalid_tool
        nr_exec_base::base_execute(self, req).await
    }
}

pub fn create(provider: &str) -> Arc<dyn nr_core::Executor> {
    Arc::new(KiroExecutor::new(provider))
}
