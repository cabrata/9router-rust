//! Port of open-sse/executors/vertex.js.
//!
//! Google Cloud Vertex AI: Supports both regional Gemini models and
//! partner models via OpenAI-compatible gateway. Auth: SA JSON (JWT assertion)
//! or ADC user credentials (OAuth2 refresh).

use std::sync::{Arc, LazyLock, Mutex};
use std::collections::HashMap;

use async_trait::async_trait;
use nr_core::{ExecRequest, ExecResult, Executor, Result, Credentials, Json};
use nr_exec_base::base::{base_execute, BaseHooks};
use serde_json::json;

static EMPTY: LazyLock<Json> = LazyLock::new(|| Json::Object(Default::default()));

// Cache for resolved project IDs: { apiKey → projectId }
static PROJECT_ID_CACHE: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Parse Service Account JSON from apiKey string.
fn parse_vertex_sa_json(api_key: &str) -> Option<Json> {
    if api_key.trim().is_empty() {
        return None;
    }
    match serde_json::from_str::<serde_json::Value>(api_key) {
        Ok(parsed) => {
            if parsed.get("type").and_then(|t| t.as_str()) == Some("service_account")
                && parsed.get("client_email").is_some()
                && parsed.get("private_key").is_some()
                && parsed.get("project_id").is_some() {
                Some(parsed)
            } else {
                None
            }
        }
        Err(_) => None,
    }
}

/// Parse Google ADC user credential JSON from apiKey string.
/// This is the format produced by `gcloud auth application-default login`.
fn parse_vertex_adc_json(api_key: &str) -> Option<Json> {
    if api_key.trim().is_empty() {
        return None;
    }
    match serde_json::from_str::<serde_json::Value>(api_key) {
        Ok(parsed) => {
            if parsed.get("type").and_then(|t| t.as_str()) == Some("authorized_user")
                && parsed.get("client_id").is_some()
                && parsed.get("client_secret").is_some()
                && parsed.get("refresh_token").is_some() {
                Some(parsed)
            } else {
                None
            }
        }
        Err(_) => None,
    }
}

struct VertexExecutor {
    provider: String,
    config: &'static Json,
}

impl VertexExecutor {
    fn new(provider_id: &str) -> Self {
        let config = nr_data::provider(provider_id).or_else(|| nr_data::provider("openai")).unwrap_or(&EMPTY);
        Self {
            provider: provider_id.to_string(),
            config,
        }
    }

    /// Get the project ID from SA JSON, ADC JSON, or credentials.
    fn get_project_id(&self, creds: &Credentials, sa_json: Option<&Json>, adc_json: Option<&Json>) -> Option<String> {
        sa_json.and_then(|j| j.get("project_id").and_then(Json::as_str).map(String::from))
            .or_else(|| adc_json.and_then(|j| j.get("quota_project_id").and_then(Json::as_str).map(String::from)))
            .or_else(|| creds.psd_str("projectId").map(String::from))
    }

    /// Check if using OAuth (SA JSON, ADC, or pre-set accessToken).
    fn uses_oauth(sa_json: Option<&Json>, adc_json: Option<&Json>, access_token: Option<&str>) -> bool {
        sa_json.is_some() || adc_json.is_some() || access_token.is_some()
    }
}

impl BaseHooks for VertexExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn config(&self) -> &Json {
        self.config
    }

    fn build_url(&self, model: &str, stream: bool, _url_index: usize, creds: &Credentials) -> Result<String> {
        let api_key = creds.api_key().unwrap_or("");
        let sa_json = parse_vertex_sa_json(api_key);
        let adc_json = parse_vertex_adc_json(api_key);
        let uses_oauth = Self::uses_oauth(sa_json.as_ref(), adc_json.as_ref(), creds.access_token());
        let raw_key = if uses_oauth { None } else { creds.api_key() };
        let project_id = self.get_project_id(creds, sa_json.as_ref(), adc_json.as_ref());

        if self.provider == "vertex-partner" {
            // Partner models require project_id in path regardless of auth method
            let project = project_id.ok_or_else(|| nr_core::Error::InvalidConfig(
                "Vertex partner models require a project_id. Add it in providerSpecificData or use Service Account JSON.".into()
            ))?;
            let url = format!("https://aiplatform.googleapis.com/v1/projects/{}/locations/global/endpoints/openapi/chat/completions", project);
            return if let Some(key) = raw_key {
                Ok(format!("{}?key={}", url, key))
            } else {
                Ok(url)
            };
        }

        // Gemini on Vertex
        let action = if stream { "streamGenerateContent" } else { "generateContent" };

        if uses_oauth {
            // SA JSON / ADC / pre-set accessToken: must use project-scoped path
            let project = project_id.ok_or_else(|| nr_core::Error::InvalidConfig(
                "Vertex OAuth/ADC requires a project_id. Add quota_project_id to your ADC JSON or set providerSpecificData.projectId.".into()
            ))?;

            // Get location from psd or model metadata or default
            let location = creds.psd_str("location")
                .unwrap_or("us-central1");

            let mut url = format!(
                "https://aiplatform.googleapis.com/v1/projects/{}/locations/{}/publishers/google/models/{}:{}",
                project, location, model, action
            );
            if stream {
                url.push_str("?alt=sse");
            }
            Ok(url)
        } else {
            // Raw API keys: projectless endpoint, or project-scoped if configured
            let location = creds.psd_str("location").unwrap_or("global");
            let url = if let Some(project) = project_id {
                format!(
                    "https://aiplatform.googleapis.com/v1/projects/{}/locations/{}/publishers/google/models/{}:{}",
                    project, location, model, action
                )
            } else {
                format!(
                    "https://aiplatform.googleapis.com/v1/publishers/google/models/{}:{}",
                    model, action
                )
            };

            let mut result = url;
            if stream {
                result.push_str("?alt=sse");
            }
            if let Some(key) = raw_key {
                result.push_str(if stream { "&key=" } else { "?key=" });
                result.push_str(key);
            }
            Ok(result)
        }
    }

    fn build_headers(&self, creds: &Credentials, stream: bool, _url: &str, _model: &str, _body: &Json) -> Result<nr_core::http::HeaderList> {
        let mut headers = nr_core::http::HeaderList::new();
        headers.insert("Content-Type".into(), "application/json".into());

        // Only set Bearer token if using SA JSON / ADC flow (raw key goes in URL ?key=)
        if let Some(token) = creds.access_token() {
            headers.insert("Authorization".into(), format!("Bearer {}", token));
        }

        if stream {
            headers.insert("Accept".into(), "text/event-stream".into());
        }

        Ok(headers)
    }
}

#[async_trait]
impl Executor for VertexExecutor {
    fn provider(&self) -> &str {
        &self.provider
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
    Arc::new(VertexExecutor::new(provider))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_vertex_sa_json_valid() {
        let json_str = r#"{"type":"service_account","client_email":"test@example.com","private_key":"key","project_id":"proj"}"#;
        let result = parse_vertex_sa_json(json_str);
        assert!(result.is_some());
    }

    #[test]
    fn test_parse_vertex_sa_json_invalid_missing_field() {
        let json_str = r#"{"type":"service_account","client_email":"test@example.com"}"#;
        let result = parse_vertex_sa_json(json_str);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_vertex_sa_json_wrong_type() {
        let json_str = r#"{"type":"authorized_user","client_id":"id"}"#;
        let result = parse_vertex_sa_json(json_str);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_vertex_adc_json_valid() {
        let json_str = r#"{"type":"authorized_user","client_id":"id","client_secret":"secret","refresh_token":"token"}"#;
        let result = parse_vertex_adc_json(json_str);
        assert!(result.is_some());
    }

    #[test]
    fn test_parse_vertex_adc_json_invalid() {
        let json_str = r#"{"type":"service_account","client_email":"test@example.com"}"#;
        let result = parse_vertex_adc_json(json_str);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_vertex_empty_string() {
        assert!(parse_vertex_sa_json("").is_none());
        assert!(parse_vertex_adc_json("").is_none());
    }

    #[test]
    fn test_uses_oauth() {
        let sa = parse_vertex_sa_json(r#"{"type":"service_account","client_email":"t","private_key":"k","project_id":"p"}"#);
        assert!(VertexExecutor::uses_oauth(sa.as_ref(), None, None));
        assert!(VertexExecutor::uses_oauth(None, None, Some("token")));
        assert!(!VertexExecutor::uses_oauth(None, None, None));
    }
}
