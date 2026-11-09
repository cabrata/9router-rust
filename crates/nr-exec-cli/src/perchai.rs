//! Port of open-sse/executors/perchai.js.
//!
//! Perch AI Executor — routes chat completions to Perch AI via Responses API.
//! Auth: Bearer token from OAuth or ~/.perch/cli-auth-session.json.
//!
//! Perch-specific logic: plan gate checking, turn ticket management, session refresh,
//! stream transformation (Perch format → OpenAI SSE format).

use async_trait::async_trait;
use nr_core::http::{HeaderList, ProxyOptions};
use nr_core::{Credentials, Error, ExecRequest, ExecResult, Json, Result, Executor};
use nr_exec_base::base::BaseHooks;
use std::sync::Arc;

/// Perch AI Executor — routes chat completions to Perch AI.
/// Auth: Bearer token from OAuth or ~/.perch/cli-auth-session.json
pub struct PerchAiExecutor {
    provider: String,
    config: Json,
}

impl PerchAiExecutor {
    pub fn new() -> Self {
        let provider = "perchai".to_string();
        let config = nr_data::provider("perchai")
            .cloned()
            .unwrap_or_else(|| Json::Object(Default::default()));
        Self { provider, config }
    }
}

impl Default for PerchAiExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BaseHooks for PerchAiExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn config(&self) -> &Json {
        &self.config
    }

    fn build_url(&self, _model: &str, _stream: bool, url_index: usize, creds: &Credentials) -> Result<String> {
        let app_url = creds.psd_str("appUrl")
            .or_else(|| creds.str("appUrl"))
            .unwrap_or("https://app.perchai.app");
        Ok(format!("{}/api/perch-terminal/model-call", app_url.trim_end_matches('/')))
    }

    fn build_headers(
        &self,
        creds: &Credentials,
        stream: bool,
        _url: &str,
        _model: &str,
        _body: &Json,
    ) -> Result<HeaderList> {
        let mut headers = HeaderList::new();
        headers.insert("Content-Type".into(), "application/json".into());
        headers.insert("User-Agent".into(), "perchai-cli/2.4.118".into());
        headers.insert("x-perch-locale".into(), "en".into());

        if let Some(token) = creds.bearer() {
            headers.insert("Authorization".into(), format!("Bearer {token}"));
        }
        if stream {
            headers.insert("Accept".into(), "text/event-stream".into());
        }
        Ok(headers)
    }

    fn transform_request(&self, _model: &str, body: &Json, _stream: bool, _creds: &Credentials) -> Result<Json> {
        // Perch-specific request transformation (simplified; full version in JS handles plan gates, etc.)
        Ok(body.clone())
    }
}

#[async_trait]
impl Executor for PerchAiExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        // Perch has complex custom logic (plan gates, session refresh, stream transformation).
        // For now, delegate to base_execute. Full implementation would:
        // 1. Check plan state
        // 2. Get turn ticket
        // 3. Refresh session if needed
        // 4. Transform response stream (Perch format → OpenAI SSE)
        nr_exec_base::base::base_execute(self, req).await
    }
}

/// Executor for the canonical provider id (aliases are resolved in nr-executors).
pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(PerchAiExecutor::new())
}
