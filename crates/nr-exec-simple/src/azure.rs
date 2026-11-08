//! Port of open-sse/executors/azure.js.

use std::sync::Arc;
use std::sync::LazyLock;

use async_trait::async_trait;
use nr_core::{ExecRequest, ExecResult, Executor, Result, Credentials, Json};
use nr_exec_base::base::{base_execute, BaseHooks};

static EMPTY: LazyLock<Json> = LazyLock::new(|| Json::Object(Default::default()));

struct AzureExecutor {
    config: &'static Json,
}

impl AzureExecutor {
    fn new() -> Self {
        let config = nr_data::provider("azure").or_else(|| nr_data::provider("openai")).unwrap_or(&EMPTY);
        Self { config }
    }
}

impl BaseHooks for AzureExecutor {
    fn provider(&self) -> &str {
        "azure"
    }

    fn config(&self) -> &nr_core::Json {
        self.config
    }

    fn build_url(&self, model: &str, _stream: bool, _url_index: usize, creds: &Credentials) -> Result<String> {
        let azure_endpoint = creds.psd_str("azureEndpoint")
            .or_else(|| std::env::var("AZURE_ENDPOINT").ok().as_deref())
            .unwrap_or("https://api.openai.com");

        let api_version = creds.psd_str("apiVersion")
            .or_else(|| std::env::var("AZURE_API_VERSION").ok().as_deref())
            .unwrap_or("2024-10-01-preview");

        let deployment = creds.psd_str("deployment")
            .or(Some(model))
            .or_else(|| std::env::var("AZURE_DEPLOYMENT").ok().as_deref())
            .unwrap_or("gpt-4");

        let endpoint = azure_endpoint.trim_end_matches('/');
        Ok(format!("{}/openai/deployments/{}/chat/completions?api-version={}", endpoint, deployment, api_version))
    }

    fn build_headers(&self, creds: &Credentials, stream: bool, _url: &str, _model: &str, _body: &Json) -> Result<nr_core::http::HeaderList> {
        let mut headers = nr_core::http::HeaderList::new();
        headers.insert("Content-Type".into(), "application/json".into());

        // Add config headers
        if let Some(cfg_headers) = self.config.get("headers").and_then(|h| h.as_object()) {
            for (k, v) in cfg_headers {
                if let Some(v_str) = v.as_str() {
                    headers.insert(k.clone(), v_str.to_string());
                }
            }
        }

        let api_key = creds.api_key()
            .or_else(|| creds.access_token())
            .or_else(|| std::env::var("OPENAI_API_KEY").ok().as_deref());

        if let Some(key) = api_key {
            headers.insert("api-key".into(), key.to_string());
        }

        let organization = creds.psd_str("organization")
            .or_else(|| std::env::var("AZURE_ORGANIZATION").ok().as_deref());
        if let Some(org) = organization {
            headers.insert("OpenAI-Organization".into(), org.to_string());
        }

        if stream {
            headers.insert("Accept".into(), "text/event-stream".into());
        }

        Ok(headers)
    }
}

#[async_trait]
impl Executor for AzureExecutor {
    fn provider(&self) -> &str {
        "azure"
    }

    fn no_auth(&self) -> bool {
        BaseHooks::no_auth(self)
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        base_execute(self, req).await
    }
}

/// Executor for the canonical provider id (aliases are resolved in nr-executors).
pub fn create(provider: &str) -> Arc<dyn Executor> {
    Arc::new(AzureExecutor::new())
}
