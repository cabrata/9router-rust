//! Port of open-sse/executors/perchai.js.

use async_trait::async_trait;
use nr_core::http::HeaderList;
use nr_core::{Credentials, ExecRequest, ExecResult, Json, Result, Executor};
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

    fn build_url(&self, _model: &str, _stream: bool, url_index: usize, _creds: &Credentials) -> Result<String> {
        Ok(nr_exec_base::base::default_build_url(self, url_index, _creds))
    }

    fn build_headers(
        &self,
        creds: &Credentials,
        stream: bool,
        _url: &str,
        _model: &str,
        _body: &Json,
    ) -> Result<HeaderList> {
        Ok(nr_exec_base::base::default_build_headers(self, creds, stream))
    }

    fn transform_request(&self, _model: &str, body: &Json, _stream: bool, _creds: &Credentials) -> Result<Json> {
        Ok(body.clone())
    }
}

#[async_trait]
impl Executor for PerchAiExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        nr_exec_base::base::base_execute(self, req).await
    }
}

/// Executor for the canonical provider id (aliases are resolved in nr-executors).
pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(PerchAiExecutor::new())
}
