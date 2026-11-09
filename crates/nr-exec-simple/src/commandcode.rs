//! Port of open-sse/executors/commandcode.js + shared/commandCodeHeaders.js.
//! Handles CommandCode NDJSON upstream (no `data:` prefix), translates to OpenAI SSE.
//! shortcut: x-session-id slugification uses simple rules instead of full @sindresorhus/slugify

use std::sync::Arc;
use std::sync::LazyLock;
use async_trait::async_trait;
use nr_core::{Credentials, ExecRequest, ExecResult, Executor, Json, Result};
use nr_core::http::HeaderList;
use nr_exec_base::base::{base_execute, BaseHooks};

static EMPTY: LazyLock<Json> = LazyLock::new(|| Json::Object(Default::default()));

/// Simple slugify: lowercase, replace non-alphanumeric with dash, trim dashes
fn slugify_path(value: &str) -> String {
    value
        .to_lowercase()
        .replace(|c: char| !c.is_alphanumeric() && c != '-' && c != '_', "-")
        .replace("--", "-")
        .trim_matches('-')
        .to_string()
}

pub struct CommandCodeExecutor {
    config: &'static Json,
}

impl CommandCodeExecutor {
    pub fn new() -> Self {
        let config = nr_data::provider("commandcode").or_else(|| nr_data::provider("openai")).unwrap_or(&EMPTY);
        Self { config }
    }
}

#[async_trait]
impl BaseHooks for CommandCodeExecutor {
    fn provider(&self) -> &str {
        "commandcode"
    }

    fn config(&self) -> &Json {
        self.config
    }

    fn build_headers(&self, creds: &Credentials, stream: bool, _url: &str, _model: &str, body: &Json) -> Result<HeaderList> {
        let mut headers = HeaderList::new();
        headers.insert("Content-Type".into(), "application/json".into());

        // Add provider config headers
        if let Some(config_headers) = self.config.get("headers").and_then(Json::as_object) {
            for (k, v) in config_headers {
                if let Some(v_str) = v.as_str() {
                    headers.insert(k.clone(), v_str.to_string());
                }
            }
        }

        // Auth header
        if let Some(key) = creds.api_key().or_else(|| creds.access_token()) {
            headers.insert("Authorization".into(), format!("Bearer {}", key));
        }

        // Session ID from body.threadId (stable per conversation)
        if let Some(thread_id) = body.get("threadId").and_then(Json::as_str) {
            headers.insert("x-session-id".into(), thread_id.to_string());
        }

        // Project slug from current working dir (matches official CLI)
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(|s| s.to_string()))
            .unwrap_or_else(|| ".".to_string());
        headers.insert("x-project-slug".into(), slugify_path(&cwd));

        if stream {
            headers.insert("Accept".into(), "text/event-stream".into());
        }

        Ok(headers)
    }

    fn transform_request(&self, _model: &str, body: &Json, _stream: bool, _creds: &Credentials) -> Result<Json> {
        let mut result = body.clone();

        // Force streaming (CommandCode requires it)
        if let Some(obj) = result.as_object_mut() {
            obj.insert("stream".into(), Json::Bool(true));
            if let Some(params) = obj.get_mut("params") {
                if let Some(params_obj) = params.as_object_mut() {
                    params_obj.insert("stream".into(), Json::Bool(true));
                }
            }
        }

        Ok(result)
    }
}

#[async_trait]
impl Executor for CommandCodeExecutor {
    fn provider(&self) -> &str {
        "commandcode"
    }

    fn no_auth(&self) -> bool {
        BaseHooks::no_auth(self)
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        base_execute(self, req).await
    }
}

pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(CommandCodeExecutor::new())
}
