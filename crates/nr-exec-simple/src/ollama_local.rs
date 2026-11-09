//! Port of open-sse/executors/ollama-local.js — Ollama local inference.

use std::sync::Arc;
use std::sync::LazyLock;

use async_trait::async_trait;
use nr_core::{ExecRequest, ExecResult, Executor, Result, Credentials, Json};
use nr_exec_base::base::{base_execute, BaseHooks};

static EMPTY: LazyLock<Json> = LazyLock::new(|| Json::Object(Default::default()));

fn resolve_ollama_local_host(creds: &Credentials) -> String {
    creds.psd_str("host")
        .map(String::from)
        .or_else(|| std::env::var("OLLAMA_HOST").ok())
        .unwrap_or_else(|| "http://localhost:11434".to_string())
}

struct OllamaLocalExecutor {
    config: &'static Json,
}

impl OllamaLocalExecutor {
    fn new() -> Self {
        let config = nr_data::provider("ollama-local").or_else(|| nr_data::provider("openai")).unwrap_or(&EMPTY);
        Self { config }
    }
}

impl BaseHooks for OllamaLocalExecutor {
    fn provider(&self) -> &str {
        "ollama-local"
    }

    fn config(&self) -> &Json {
        self.config
    }

    fn build_url(&self, _model: &str, _stream: bool, _url_index: usize, creds: &Credentials) -> Result<String> {
        let host = resolve_ollama_local_host(creds);
        Ok(format!("{}/api/chat", host.trim_end_matches('/')))
    }
}

#[async_trait]
impl Executor for OllamaLocalExecutor {
    fn provider(&self) -> &str {
        "ollama-local"
    }

    fn no_auth(&self) -> bool {
        BaseHooks::no_auth(self)
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        base_execute(self, req).await
    }
}

/// Executor for the canonical provider id (aliases are resolved in nr-executors).
pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(OllamaLocalExecutor::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_resolve_ollama_local_host_default() {
        let creds = Credentials::from_value(json!({}));
        let host = resolve_ollama_local_host(&creds);
        assert_eq!(host, "http://localhost:11434");
    }

    #[test]
    fn test_resolve_ollama_local_host_from_psd() {
        let creds = Credentials::from_value(json!({"providerSpecificData": {"host": "http://192.168.1.100:11434"}}));
        let host = resolve_ollama_local_host(&creds);
        assert_eq!(host, "http://192.168.1.100:11434");
    }
}
