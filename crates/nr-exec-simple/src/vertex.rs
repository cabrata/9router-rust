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

/// Resolve GCP project ID from a raw Vertex API key.
/// Sends a dummy 404 request and parses "projects/{id}" from the error message.
async fn resolve_project_id(api_key: &str, proxy: Option<&nr_core::http::ProxyOptions>) -> Option<String> {
    {
        let cache = PROJECT_ID_CACHE.lock().unwrap();
        if let Some(id) = cache.get(api_key) {
            return Some(id.clone());
        }
    }

    let url = format!(
        "https://aiplatform.googleapis.com/v1/publishers/google/models/__probe__:generateContent?key={}",
        api_key
    );
    let mut headers = nr_core::http::HeaderList::new();
    headers.insert("Content-Type".into(), "application/json".into());
    let fetch = nr_core::http::FetchRequest::post_json(url, headers, &serde_json::json!({}));
    let res = nr_core::http::proxy_fetch(fetch, proxy).await.ok()?;
    let bytes = res.bytes().await.ok()?;
    let json: Option<serde_json::Value> = serde_json::from_slice(&bytes).ok();

    let msg = json.as_ref()
        .and_then(|j| {
            j.get(0).and_then(|e| e.get("error")).and_then(|e| e.get("message")).and_then(Json::as_str)
                .or_else(|| j.get("error").and_then(|e| e.get("message")).and_then(Json::as_str))
        })
        .unwrap_or("");

    let re = regex::Regex::new(r"projects/([^/]+)/").ok()?;
    let project_id = re.captures(msg)?.get(1)?.as_str().to_string();

    let mut cache = PROJECT_ID_CACHE.lock().unwrap();
    cache.insert(api_key.to_string(), project_id.clone());
    Some(project_id)
}

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
            let project = project_id.ok_or_else(|| nr_core::Error::Config(
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
            let project = project_id.ok_or_else(|| nr_core::Error::Config(
                "Vertex OAuth/ADC requires a project_id. Add quota_project_id to your ADC JSON or set providerSpecificData.projectId.".into()
            ))?;

            // Global-only models (registry defaultLocation, e.g. Argon) must not
            // fall back to a regional location.
            let location = creds
                .psd_str("location")
                .map(str::to_string)
                .or_else(|| {
                    nr_data::get_model_meta(&self.provider, model).and_then(|m| {
                        m.get("defaultLocation").and_then(Json::as_str).map(str::to_string)
                    })
                })
                .unwrap_or_else(|| "us-central1".to_string());

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

#[cfg(test)]
mod shaping_tests {
    use super::*;
    use serde_json::json;

    fn creds(v: serde_json::Value) -> Credentials {
        Credentials::from_value(v)
    }

    #[test]
    fn raw_key_url_is_projectless_with_key_query() {
        let ex = VertexExecutor::new("vertex");
        let c = creds(json!({"apiKey": "AIza-key"}));
        let url = ex.build_url("gemini-2.5-pro", false, 0, &c).unwrap();
        assert_eq!(
            url,
            "https://aiplatform.googleapis.com/v1/publishers/google/models/gemini-2.5-pro:generateContent?key=AIza-key"
        );
    }

    #[test]
    fn streaming_raw_key_appends_alt_sse_before_key() {
        let ex = VertexExecutor::new("vertex");
        let c = creds(json!({"apiKey": "AIza-key"}));
        let url = ex.build_url("gemini-2.5-pro", true, 0, &c).unwrap();
        assert_eq!(
            url,
            "https://aiplatform.googleapis.com/v1/publishers/google/models/gemini-2.5-pro:streamGenerateContent?alt=sse&key=AIza-key"
        );
    }

    #[test]
    fn oauth_uses_project_scoped_path_and_bearer() {
        let ex = VertexExecutor::new("vertex");
        let c = creds(json!({
            "apiKey": "AIza-key",
            "accessToken": "ya29.token",
            "providerSpecificData": {"projectId": "proj-1"},
        }));
        let url = ex.build_url("gemini-2.5-pro", true, 0, &c).unwrap();
        assert_eq!(
            url,
            "https://aiplatform.googleapis.com/v1/projects/proj-1/locations/us-central1/publishers/google/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
        );
        let headers = ex.build_headers(&c, true, &url, "gemini-2.5-pro", &json!({})).unwrap();
        assert_eq!(headers["Authorization"], "Bearer ya29.token");
        assert_eq!(headers["Accept"], "text/event-stream");
    }

    #[test]
    fn oauth_requires_a_project() {
        let ex = VertexExecutor::new("vertex");
        let c = creds(json!({"apiKey": "AIza-key", "accessToken": "ya29.token"}));
        let err = ex.build_url("gemini-2.5-pro", false, 0, &c).unwrap_err();
        assert!(err.to_string().contains("project_id"), "got {err}");
    }

    #[test]
    fn explicit_location_wins_over_default() {
        let ex = VertexExecutor::new("vertex");
        let c = creds(json!({
            "accessToken": "t",
            "providerSpecificData": {"projectId": "p", "location": "europe-west1"},
        }));
        let url = ex.build_url("gemini-2.5-pro", false, 0, &c).unwrap();
        assert!(url.contains("/locations/europe-west1/"), "got {url}");
    }

    #[test]
    fn global_only_model_ignores_regional_default() {
        // gemini-4-argon is the registry's `defaultLocation: "global"` model.
        let ex = VertexExecutor::new("vertex");
        let c = creds(json!({"accessToken": "t", "providerSpecificData": {"projectId": "p"}}));
        let url = ex.build_url("gemini-4-argon", false, 0, &c).unwrap();
        assert!(url.contains("/locations/global/"), "got {url}");
    }

    #[test]
    fn raw_key_honours_configured_project_and_location() {
        let ex = VertexExecutor::new("vertex");
        let c = creds(json!({
            "apiKey": "AIza-key",
            "providerSpecificData": {"projectId": "p2", "location": "asia-northeast1"},
        }));
        let url = ex.build_url("gemini-2.5-pro", false, 0, &c).unwrap();
        assert_eq!(
            url,
            "https://aiplatform.googleapis.com/v1/projects/p2/locations/asia-northeast1/publishers/google/models/gemini-2.5-pro:generateContent?key=AIza-key"
        );
    }

    #[test]
    fn partner_requires_project_and_uses_global_openapi_endpoint() {
        let ex = VertexExecutor::new("vertex-partner");
        let missing = creds(json!({"apiKey": "AIza-key"}));
        assert!(ex.build_url("llama-4", false, 0, &missing).is_err());

        let c = creds(json!({"apiKey": "AIza-key", "providerSpecificData": {"projectId": "p3"}}));
        assert_eq!(
            ex.build_url("llama-4", false, 0, &c).unwrap(),
            "https://aiplatform.googleapis.com/v1/projects/p3/locations/global/endpoints/openapi/chat/completions?key=AIza-key"
        );
    }

    #[test]
    fn partner_with_oauth_omits_key_query() {
        let ex = VertexExecutor::new("vertex-partner");
        let c = creds(json!({
            "accessToken": "tok",
            "providerSpecificData": {"projectId": "p4"},
        }));
        assert_eq!(
            ex.build_url("llama-4", false, 0, &c).unwrap(),
            "https://aiplatform.googleapis.com/v1/projects/p4/locations/global/endpoints/openapi/chat/completions"
        );
    }
}
