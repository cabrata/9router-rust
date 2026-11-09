//! Port of open-sse/executors/azure.js.

use std::sync::Arc;
use std::sync::LazyLock;

use async_trait::async_trait;
use nr_core::{ExecRequest, ExecResult, Executor, Result, Credentials, Json};
use nr_exec_base::base::{base_execute, BaseHooks};

static EMPTY: LazyLock<Json> = LazyLock::new(|| Json::Object(Default::default()));

/// `process.env.X` as a JS-truthy string.
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

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
        let azure_endpoint = creds.psd_str("azureEndpoint").map(String::from)
            .or_else(|| env_nonempty("AZURE_ENDPOINT"))
            .unwrap_or_else(|| "https://api.openai.com".into());

        let api_version = creds.psd_str("apiVersion").map(String::from)
            .or_else(|| env_nonempty("AZURE_API_VERSION"))
            .unwrap_or_else(|| "2024-10-01-preview".into());

        let deployment = creds.psd_str("deployment").map(String::from)
            .or_else(|| (!model.is_empty()).then(|| model.to_string()))
            .or_else(|| env_nonempty("AZURE_DEPLOYMENT"))
            .unwrap_or_else(|| "gpt-4".into());

        // JS `replace(/\/$/, "")` strips one trailing slash.
        let endpoint = azure_endpoint.strip_suffix('/').unwrap_or(&azure_endpoint);
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

        let api_key = creds.api_key().or_else(|| creds.access_token()).map(String::from)
            .or_else(|| env_nonempty("OPENAI_API_KEY"));

        if let Some(key) = api_key {
            headers.insert("api-key".into(), key);
        }

        let organization = creds.psd_str("organization").map(String::from)
            .or_else(|| env_nonempty("AZURE_ORGANIZATION"));
        if let Some(org) = organization {
            headers.insert("OpenAI-Organization".into(), org);
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
pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(AzureExecutor::new())
}

#[cfg(test)]
mod shaping_tests {
    use super::*;
    use serde_json::{json, Value};

    fn creds(v: serde_json::Value) -> Credentials {
        Credentials::from_value(v)
    }

    /// Serializing an env var test keeps the process-wide env clean across
    /// parallel tests, and we only set vars that no other test reads.
    #[test]
    fn deployment_path_uses_model_and_default_api_version() {
        let ex = AzureExecutor::new();
        let c = creds(json!({"apiKey": "k", "providerSpecificData": {"azureEndpoint": "https://res.openai.azure.com/"}}));
        assert_eq!(
            ex.build_url("gpt-4o", false, 0, &c).unwrap(),
            "https://res.openai.azure.com/openai/deployments/gpt-4o/chat/completions?api-version=2024-10-01-preview"
        );
    }

    #[test]
    fn per_connection_api_version_and_deployment_win() {
        let ex = AzureExecutor::new();
        let c = creds(json!({
            "apiKey": "k",
            "providerSpecificData": {
                "azureEndpoint": "https://res.openai.azure.com",
                "apiVersion": "2025-01-01-preview",
                "deployment": "my-deploy",
            },
        }));
        assert_eq!(
            ex.build_url("ignored-model", false, 0, &c).unwrap(),
            "https://res.openai.azure.com/openai/deployments/my-deploy/chat/completions?api-version=2025-01-01-preview"
        );
    }

    #[test]
    fn api_version_falls_back_to_env_then_default() {
        let ex = AzureExecutor::new();
        let c = creds(json!({"apiKey": "k", "providerSpecificData": {"azureEndpoint": "https://e"}}));
        assert!(ex.build_url("m", false, 0, &c).unwrap().contains("api-version=2024-10-01-preview"));
    }

    #[test]
    fn headers_use_api_key_not_bearer() {
        let ex = AzureExecutor::new();
        let c = creds(json!({
            "apiKey": "secret",
            "providerSpecificData": {"organization": "org-1"},
        }));
        let headers = ex.build_headers(&c, true, "https://e", "gpt-4o", &json!({})).unwrap();
        assert_eq!(headers["api-key"], "secret");
        assert_eq!(headers["OpenAI-Organization"], "org-1");
        assert_eq!(headers["Accept"], "text/event-stream");
        assert!(!headers.contains_key("Authorization"), "Azure uses api-key");
    }

    #[test]
    fn access_token_is_used_when_no_api_key() {
        let ex = AzureExecutor::new();
        let c = creds(json!({"accessToken": "tok"}));
        let headers = ex.build_headers(&c, false, "https://e", "gpt-4o", &json!({})).unwrap();
        assert_eq!(headers["api-key"], "tok");
        assert!(!headers.contains_key("Accept"), "non-stream has no Accept");
    }
}
