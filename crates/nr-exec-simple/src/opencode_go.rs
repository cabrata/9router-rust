//! Port of open-sse/executors/opencode-go.js.
//!
//! OpenCode Go handles session management for Muse Spark and other responses-format models.
//! TODO(dedupe): Implement resolveSessionId and getModelTargetFormat from utils/sessionManager.js
//! and config/providerModels.js once ported.

use std::sync::Arc;

use async_trait::async_trait;
use nr_core::{ExecRequest, ExecResult, Executor, Json, Result, Credentials};
use nr_exec_base::base::{base_execute, BaseHooks};
use std::sync::LazyLock;

const RESPONSES_BASE_URL: &str = "https://opencode.ai/inference/go/openai/v1/responses";
const SESSION_HEADER: &str = "x-opencode-session";
const SESSION_FIELD: &str = "_opencodeGoSession";

static EMPTY: LazyLock<Json> = LazyLock::new(|| Json::Object(Default::default()));

/// Translate a session ID to opencode-go format.
fn translate_session_id(session_id: &str, client_tool: &str) -> String {
    use sha2::{Sha256, Digest};

    let key = format!("opencode-go\0{}\0{}", client_tool, session_id);
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();
    let hex: String = hex::encode(&digest[..16]);
    format!("ses_{}", hex)
}

/// Normalize session value.
fn normalize_session(value: &str) -> Option<String> {
    let normalized = value.trim();
    if normalized.is_empty() || normalized.len() > 256 {
        return None;
    }
    Some(normalized.to_string())
}

/// Extract native session from headers.
fn native_session(credentials: &Credentials) -> Option<String> {
    let headers = credentials.get("rawHeaders")
        .and_then(|h| h.as_object())?;

    for (k, v) in headers {
        if k.to_lowercase() == SESSION_HEADER {
            if let Some(v_str) = v.as_str() {
                return normalize_session(v_str);
            }
        }
    }
    None
}

struct OpencodeGoExecutor {
    config: &'static Json,
}

impl OpencodeGoExecutor {
    fn new() -> Self {
        let config = nr_data::provider("opencode-go")
            .or_else(|| nr_data::provider("openai"))
            .unwrap_or(&EMPTY);
        Self { config }
    }

    fn prepare_request_credentials(
        &self,
        body: &Json,
        credentials: &Credentials,
        provider_session_id: Option<&str>,
        client_tool: Option<&str>,
    ) -> Credentials {
        let mut creds = credentials.clone();

        // Try to get native session from headers
        if let Some(native) = native_session(&creds) {
            creds.set(SESSION_FIELD, Json::String(native));
            return creds;
        }

        // TODO: Implement resolveSessionId from utils/sessionManager.js
        let resolved = provider_session_id
            .and_then(|s| normalize_session(s))
            .unwrap_or_else(|| "default-session".to_string());

        let translated = translate_session_id(&resolved, client_tool.unwrap_or("generic"));
        creds.set(SESSION_FIELD, Json::String(translated));
        creds
    }
}

impl BaseHooks for OpencodeGoExecutor {
    fn provider(&self) -> &str {
        "opencode-go"
    }

    fn config(&self) -> &Json {
        self.config
    }

    fn build_url(
        &self,
        model: &str,
        _stream: bool,
        _url_index: usize,
        _creds: &Credentials,
    ) -> Result<String> {
        // TODO: Implement isResponsesModel check from provider models
        // For now, assume muse-spark models use responses
        if model.contains("muse-spark") || model.contains("grok-4.6") || model.contains("gpt-5.6") {
            Ok(RESPONSES_BASE_URL.to_string())
        } else {
            Ok(format!("{}/chat/completions", "https://opencode.ai/inference/go/openai/v1"))
        }
    }

    fn build_headers(
        &self,
        credentials: &Credentials,
        stream: bool,
        _url: &str,
        _model: &str,
        _body: &Json,
    ) -> Result<nr_core::http::HeaderList> {
        let mut headers = nr_exec_base::base::default_build_headers(self, credentials, stream)?;

        if let Some(session) = credentials.get(SESSION_FIELD).and_then(Json::as_str) {
            headers.insert(SESSION_HEADER.to_string(), session.to_string());
        }

        Ok(headers)
    }
}

#[async_trait]
impl Executor for OpencodeGoExecutor {
    fn provider(&self) -> &str {
        "opencode-go"
    }

    fn no_auth(&self) -> bool {
        BaseHooks::no_auth(self)
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        // Prepare credentials with session
        let credentials = self.prepare_request_credentials(
            &req.body,
            req.credentials,
            None,
            None,
        );

        // Execute through base executor with prepared credentials
        let mut req_creds = credentials;
        base_execute(self, ExecRequest {
            credentials: &mut req_creds,
            ..req
        }).await
    }
}

/// Executor for the canonical provider id (aliases are resolved in nr-executors).
pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(OpencodeGoExecutor::new())
}
