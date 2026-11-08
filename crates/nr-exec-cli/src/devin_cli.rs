//! Port of open-sse/executors/devin-cli.js.
//! Routes completions through the official Devin CLI binary via ACP (Agent Client Protocol) JSON-RPC 2.0.

use async_trait::async_trait;
use nr_core::http::HeaderList;
use nr_core::{Credentials, ExecRequest, ExecResult, Json, Result, Executor};
use nr_exec_base::base::BaseHooks;
use std::sync::Arc;

/// Devin CLI Executor — routes completions through the official Devin CLI binary.
/// Auth: noAuth — the subprocess inherits the parent env and uses credentials
/// stored by `devin auth login` (~/.local/share/devin/credentials.toml).
pub struct DevinCliExecutor {
    provider: String,
    config: Json,
}

impl DevinCliExecutor {
    pub fn new() -> Self {
        let provider = "devin-cli".to_string();
        let config = Json::Object(Default::default());
        Self { provider, config }
    }
}

impl Default for DevinCliExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BaseHooks for DevinCliExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn config(&self) -> &Json {
        &self.config
    }

    fn no_auth(&self) -> bool {
        true
    }

    fn build_url(&self, _model: &str, _stream: bool, url_index: usize, _creds: &Credentials) -> Result<String> {
        Ok("devin://acp/stdio".to_string())
    }

    fn build_headers(
        &self,
        _creds: &Credentials,
        _stream: bool,
        _url: &str,
        _model: &str,
        _body: &Json,
    ) -> Result<HeaderList> {
        Ok(HeaderList::new())
    }

    fn transform_request(&self, _model: &str, body: &Json, _stream: bool, _creds: &Credentials) -> Result<Json> {
        Ok(body.clone())
    }
}

#[async_trait]
impl Executor for DevinCliExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn no_auth(&self) -> bool {
        true
    }

    async fn execute(&self, _req: ExecRequest<'_>) -> Result<ExecResult> {
        // TODO: implement full devin-cli execution with subprocess spawning
        Err(nr_core::Error::Other("Devin CLI executor not yet fully implemented".to_string()))
    }
}

/// Executor for the canonical provider id (aliases are resolved in nr-executors).
pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(DevinCliExecutor::new())
}
