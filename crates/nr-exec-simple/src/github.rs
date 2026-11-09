//! Port of open-sse/executors/github.js + services/copilotModels.js.
//! GitHub Copilot executor with model routing (claude → /v1/messages, others → /chat/completions or /responses).
//! shortcut: Copilot model catalog caching skipped (complex state management), queries live catalog each time

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

        // Strip reasoning_effort: "none" (models without it will reject it)
        if result.get("reasoning_effort").and_then(Json::as_str) == Some("none") {
            result.as_object_mut().map(|o| o.remove("reasoning_effort"));
        }

        // TODO(dedupe): stripUnsupportedParams from translator/concerns/paramSupport.js when ported

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
