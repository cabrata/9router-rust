//! Port of open-sse/executors/github.js + services/copilotModels.js.
//! GitHub Copilot executor with model routing (claude → /v1/messages, others → /chat/completions or /responses).

use std::sync::Arc;
use std::sync::LazyLock;
use async_trait::async_trait;
use nr_core::{Credentials, ExecRequest, ExecResult, Executor, Json, Result};
use nr_core::http::HeaderList;
use nr_exec_base::base::{base_execute, BaseHooks};

static EMPTY: LazyLock<Json> = LazyLock::new(|| Json::Object(Default::default()));

/// Copilot header constants from config
fn github_copilot() -> Json {
    nr_data::export("open-sse/config/appConstants", "GITHUB_COPILOT")
        .cloned()
        .unwrap_or(Json::Object(Default::default()))
}

pub struct GithubExecutor {
    config: &'static Json,
}

impl GithubExecutor {
    pub fn new() -> Self {
        let config = nr_data::provider("github").or_else(|| nr_data::provider("openai")).unwrap_or(&EMPTY);
        Self { config }
    }

    /// Claude models route to /v1/messages; others to /chat/completions
    fn is_claude_model(&self, model: &str) -> bool {
        model.to_lowercase().contains("claude")
    }

    /// Gemini/Claude don't support /responses endpoint
    fn supports_responses_endpoint(&self, model: &str) -> bool {
        let m = model.to_lowercase();
        !(m.contains("gemini") || m.contains("claude"))
    }

    /// Newer OpenAI models use max_completion_tokens instead of max_tokens
    fn requires_max_completion_tokens(&self, model: &str) -> bool {
        let m = model.to_lowercase();
        m.contains("gpt-5") || m.contains("o1-") || m.contains("o3-") || m.contains("o4-")
    }

    /// Sanitize messages for /chat/completions (only text and image_url parts)
    fn sanitize_for_chat_completions(&self, body: &Json) -> Json {
        let mut result = body.clone();

        if let Some(Json::Array(messages)) = result.get_mut("messages") {
            for msg in messages.iter_mut() {
                if let Json::Object(msg_obj) = msg {
                    // Skip messages without content or with null content
                    if msg_obj.get("content").is_none() || msg_obj.get("content") == Some(&Json::Null) {
                        continue;
                    }

                    // String content is fine
                    if let Some(Json::String(_)) = msg_obj.get("content") {
                        continue;
                    }

                    // Array content: filter unsupported part types
                    if let Some(Json::Array(content)) = msg_obj.get_mut("content") {
                        let mut cleaned = Vec::new();
                        for part in content.iter() {
                            if let Json::Object(p) = part {
                                if let Some(Json::String(_)) = p.get("type") {
                                    let type_str = p.get("type").and_then(Json::as_str).unwrap_or("");
                                    if type_str == "text" || type_str == "image_url" {
                                        cleaned.push(part.clone());
                                    } else {
                                        // Serialize other types as text
                                        let text = p.get("text")
                                            .or_else(|| p.get("content"))
                                            .and_then(Json::as_str)
                                            .map(|s| s.to_string())
                                            .unwrap_or_else(|| serde_json::to_string(p).unwrap_or_default());
                                        cleaned.push(serde_json::json!({"type": "text", "text": text}));
                                    }
                                }
                            }
                        }
                        if !cleaned.is_empty() {
                            *content = cleaned;
                        } else {
                            msg_obj.remove("content");
                        }
                    }
                }
            }
        }

        result
    }
}

#[async_trait]
impl BaseHooks for GithubExecutor {
    fn provider(&self) -> &str {
        "github"
    }

    fn config(&self) -> &Json {
        self.config
    }

    fn build_url(&self, model: &str, _stream: bool, _url_index: usize, _creds: &Credentials) -> Result<String> {
        // Return appropriate endpoint based on model type
        if self.is_claude_model(model) {
            // Claude models use /v1/messages
            if let Some(url) = self.config.get("messagesUrl").and_then(Json::as_str) {
                return Ok(url.to_string());
            }
        }

        // Others use /chat/completions (or /responses for known codex models)
        if let Some(url) = self.config.get("baseUrl").and_then(Json::as_str) {
            return Ok(url.to_string());
        }

        Ok("https://api.github.com/copilot_internal/v2/chat/completions".into())
    }

    fn build_headers(&self, creds: &Credentials, stream: bool, _url: &str, _model: &str, _body: &Json) -> Result<HeaderList> {
        let mut headers = HeaderList::new();

        let token = creds.psd_str("copilotToken")
            .or_else(|| creds.access_token())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "undefined".into());

        headers.insert("Authorization".into(), format!("Bearer {}", token));
        headers.insert("Content-Type".into(), "application/json".into());
        headers.insert("copilot-integration-id".into(), "vscode-chat".into());

        // Copilot version headers from config
        let copilot = github_copilot();
        if let Some(v) = copilot.get("VSCODE_VERSION").and_then(Json::as_str) {
            headers.insert("editor-version".into(), format!("vscode/{}", v));
        }
        if let Some(v) = copilot.get("COPILOT_CHAT_VERSION").and_then(Json::as_str) {
            headers.insert("editor-plugin-version".into(), format!("copilot-chat/{}", v));
        }
        if let Some(ua) = copilot.get("USER_AGENT").and_then(Json::as_str) {
            headers.insert("user-agent".into(), ua.to_string());
        }
        if let Some(av) = copilot.get("API_VERSION").and_then(Json::as_str) {
            headers.insert("x-github-api-version".into(), av.to_string());
        }

        // Request tracking
        let req_id = uuid::Uuid::new_v4().to_string();
        headers.insert("x-request-id".into(), req_id);
        headers.insert("x-vscode-user-agent-library-version".into(), "electron-fetch".into());
        headers.insert("X-Initiator".into(), "user".into());

        // Required by /v1/messages endpoint
        headers.insert("anthropic-version".into(), "2023-06-01".into());

        if stream {
            headers.insert("Accept".into(), "text/event-stream".into());
        }

        Ok(headers)
    }

    fn transform_request(&self, model: &str, body: &Json, _stream: bool, _creds: &Credentials) -> Result<Json> {
        let mut result = body.clone();

        // Map max_tokens → max_completion_tokens for newer models
        if self.requires_max_completion_tokens(model) {
            if let Some(max_tokens) = result.get("max_tokens").cloned() {
                result["max_completion_tokens"] = max_tokens;
                result.as_object_mut().map(|o| o.remove("max_tokens"));
            }
        }

        // "none" means no thinking — strip it so models that don't support "none" don't 400
        if result.get("reasoning_effort").and_then(Json::as_str) == Some("none") {
            result.as_object_mut().map(|o| o.remove("reasoning_effort"));
        }

        // Config-driven strip of params unsupported by this provider/model
        nr_translator::concerns::paramSupport::strip_unsupported_params("github", model, &mut result);

        Ok(result)
    }
}

#[async_trait]
impl Executor for GithubExecutor {
    fn provider(&self) -> &str {
        "github"
    }

    fn no_auth(&self) -> bool {
        BaseHooks::no_auth(self)
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        base_execute(self, req).await
    }
}

pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(GithubExecutor::new())
}

// ==================== LIVE MODEL CATALOG (port of services/copilotModels.js) ====================

const MODELS_URL: &str = "https://api.githubcopilot.com/models";
const MODELS_FETCH_TIMEOUT_MS: u64 = 10_000;
const MODELS_CACHE_TTL_MS: u64 = 5 * 60 * 1000;

struct CatalogEntry {
    expires_at: u64,
    models: Vec<Json>,
}

static CATALOG_CACHE: LazyLock<std::sync::Mutex<std::collections::HashMap<String, CatalogEntry>>> =
    LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// `cacheKey`: the Copilot token, else the GitHub access token.
fn catalog_cache_key(creds: &Credentials) -> String {
    creds
        .psd_str("copilotToken")
        .or_else(|| creds.access_token())
        .unwrap_or("copilot-anonymous")
        .to_string()
}

fn copilot_models_headers(token: &str) -> nr_core::http::HeaderList {
    let copilot = github_copilot();
    let mut headers = nr_core::http::HeaderList::new();
    headers.insert("Authorization".into(), format!("Bearer {token}"));
    headers.insert("Content-Type".into(), "application/json".into());
    headers.insert("Copilot-Integration-Id".into(), "vscode-chat".into());
    if let Some(v) = copilot.get("VSCODE_VERSION").and_then(Json::as_str) {
        headers.insert("editor-version".into(), format!("vscode/{v}"));
    }
    if let Some(v) = copilot.get("COPILOT_CHAT_VERSION").and_then(Json::as_str) {
        headers.insert("editor-plugin-version".into(), format!("copilot-chat/{v}"));
    }
    if let Some(ua) = copilot.get("USER_AGENT").and_then(Json::as_str) {
        headers.insert("user-agent".into(), ua.to_string());
    }
    if let Some(av) = copilot.get("API_VERSION").and_then(Json::as_str) {
        headers.insert("x-github-api-version".into(), av.to_string());
    }
    headers
}

/// `expandCatalog`: keep only chat models the account's policy enables.
fn expand_catalog(raw: &[Json]) -> Vec<Json> {
    let mut seen: Vec<String> = Vec::new();
    let mut models = Vec::new();
    for m in raw {
        if !m.is_object() {
            continue;
        }
        if m.get("capabilities").and_then(|c| c.get("type")).and_then(Json::as_str) != Some("chat") {
            continue;
        }
        if let Some(policy) = m.get("policy") {
            if policy.get("state").and_then(Json::as_str) != Some("enabled") {
                continue;
            }
        }
        let id = m.get("id").and_then(Json::as_str).unwrap_or("").to_string();
        if id.is_empty() || seen.contains(&id) {
            continue;
        }
        seen.push(id.clone());
        models.push(serde_json::json!({
            "id": id,
            "name": m.get("name").and_then(Json::as_str).unwrap_or(&id),
        }));
    }
    models
}

/// `fetchCatalogRaw`: one `/models` call, `Err(status)` on a non-2xx answer.
async fn fetch_catalog_raw(
    token: &str,
    proxy: Option<&nr_core::http::ProxyOptions>,
) -> std::result::Result<Vec<Json>, u16> {
    let mut fetch = nr_core::http::FetchRequest::get(MODELS_URL);
    fetch.headers = copilot_models_headers(token);
    fetch.header_timeout = Some(std::time::Duration::from_millis(MODELS_FETCH_TIMEOUT_MS));
    let response = nr_core::http::proxy_fetch(fetch, proxy).await.map_err(|_| 0u16)?;
    if !(200..300).contains(&response.status) {
        return Err(response.status);
    }
    let body = response.bytes().await.map_err(|_| 0u16)?;
    let data: Json = serde_json::from_slice(&body).map_err(|_| 0u16)?;
    Ok(data.get("data").and_then(Json::as_array).cloned().unwrap_or_default())
}

/// `resolveCopilotModels`: the account's live catalog, or `None` on failure.
pub async fn resolve_copilot_models(
    creds: &Credentials,
    force_refresh: bool,
    proxy: Option<&nr_core::http::ProxyOptions>,
) -> Option<Vec<Json>> {
    let token = creds
        .psd_str("copilotToken")
        .or_else(|| creds.access_token())?
        .to_string();

    let key = catalog_cache_key(creds);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    if !force_refresh {
        let cached = {
            let cache = CATALOG_CACHE.lock().ok()?;
            cache
                .get(&key)
                .filter(|entry| entry.expires_at > now)
                .map(|entry| entry.models.clone())
        };
        if let Some(models) = cached {
            return Some(models);
        }
    }

    let raw = match fetch_catalog_raw(&token, proxy).await {
        Ok(raw) => raw,
        // A stale Copilot token answers 401/403; refresh from the GitHub access
        // token and retry once.
        Err(status) if (status == 401 || status == 403) && creds.access_token().is_some() => {
            let access = creds.access_token().unwrap_or("").to_string();
            let refreshed = nr_oauth::token_refresh::refresh_copilot_token(&access, &nr_core::Logger::new())
                .await?;
            let next = refreshed.get("token").and_then(Json::as_str)?.to_string();
            match fetch_catalog_raw(&next, proxy).await {
                Ok(raw) => raw,
                Err(_) => return None,
            }
        }
        Err(_) => return None,
    };

    let models = expand_catalog(&raw);
    if models.is_empty() {
        return None;
    }

    if let Ok(mut cache) = CATALOG_CACHE.lock() {
        cache.insert(
            key,
            CatalogEntry { expires_at: now + MODELS_CACHE_TTL_MS, models: models.clone() },
        );
    }
    Some(models)
}

/// `clearCopilotModelCache`.
pub fn clear_copilot_model_cache() {
    if let Ok(mut cache) = CATALOG_CACHE.lock() {
        cache.clear();
    }
}

#[cfg(test)]
mod catalog_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keeps_only_enabled_chat_models() {
        let raw = vec![
            json!({"id": "gpt-5.5", "name": "GPT-5.5", "capabilities": {"type": "chat"}}),
            json!({"id": "embed", "capabilities": {"type": "embeddings"}}),
            json!({"id": "off", "capabilities": {"type": "chat"}, "policy": {"state": "disabled"}}),
            json!({"id": "on", "capabilities": {"type": "chat"}, "policy": {"state": "enabled"}}),
        ];
        let models = expand_catalog(&raw);
        let ids: Vec<&str> = models.iter().filter_map(|m| m["id"].as_str()).collect();
        assert_eq!(ids, vec!["gpt-5.5", "on"]);
    }

    #[test]
    fn name_falls_back_to_id() {
        let raw = vec![json!({"id": "claude-opus-4.8", "capabilities": {"type": "chat"}})];
        assert_eq!(expand_catalog(&raw)[0]["name"], "claude-opus-4.8");
    }

    #[test]
    fn dedupes_repeated_ids() {
        let raw = vec![
            json!({"id": "a", "capabilities": {"type": "chat"}}),
            json!({"id": "a", "capabilities": {"type": "chat"}}),
        ];
        assert_eq!(expand_catalog(&raw).len(), 1);
    }

    #[test]
    fn cache_key_prefers_copilot_token() {
        let c = Credentials::from_value(json!({
            "accessToken": "gho_user",
            "providerSpecificData": {"copilotToken": "copilot-tok"},
        }));
        assert_eq!(catalog_cache_key(&c), "copilot-tok");
        let plain = Credentials::from_value(json!({"accessToken": "gho_user"}));
        assert_eq!(catalog_cache_key(&plain), "gho_user");
        let anon = Credentials::from_value(json!({}));
        assert_eq!(catalog_cache_key(&anon), "copilot-anonymous");
    }

    #[test]
    fn headers_carry_the_copilot_identity() {
        let h = copilot_models_headers("tok");
        assert_eq!(h["Authorization"], "Bearer tok");
        assert_eq!(h["Copilot-Integration-Id"], "vscode-chat");
        assert!(h.contains_key("editor-version"));
        assert!(h.contains_key("x-github-api-version"));
    }
}
