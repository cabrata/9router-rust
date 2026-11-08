//! Port of open-sse/executors/grok-cli.js.

use async_trait::async_trait;
use nr_core::http::HeaderList;
use nr_core::{Credentials, ExecRequest, ExecResult, Json, Result, Executor};
use nr_exec_base::base::BaseHooks;
use std::sync::Arc;
use uuid::Uuid;

// Server-generated item id prefixes that /responses cannot resolve when store=false
const SERVER_ID_PATTERN: &str = r"^(rs|fc|resp|msg)_";

// Hosted tool types executed server-side by Grok CLI backend
const HOSTED_TOOL_TYPES: &[&str] = &[
    "web_search",
    "x_search",
    "web_search_preview",
    "file_search",
    "image_generation",
    "code_interpreter",
    "mcp",
    "local_shell",
];

// Fields accepted by cli-chat-proxy Responses API (mirrors Codex allowlist + Grok extras)
const RESPONSES_API_ALLOWLIST: &[&str] = &[
    "model",
    "input",
    "instructions",
    "tools",
    "tool_choice",
    "stream",
    "store",
    "reasoning",
    "include",
    "temperature",
    "top_p",
    "max_output_tokens",
    "parallel_tool_calls",
    "text",
    "metadata",
    "prompt_cache_key",
];

const EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh"];
const GROK_CLI_TURN_STORE_MAX: usize = 5000;

/// Count user turns in a Responses `input` array.
/// Official CLI sets x-grok-turn-idx to the 1-based conversation turn (≈ user messages).
pub fn count_grok_cli_user_turns(input: Option<&Json>) -> usize {
    let Some(arr) = input.and_then(Json::as_array) else {
        return 1;
    };
    let mut n = 0;
    for item in arr {
        if let Some(obj) = item.as_object() {
            let type_val = obj.get("type").and_then(Json::as_str).unwrap_or("");
            let role = obj.get("role").and_then(Json::as_str).unwrap_or("");
            // Responses message items (type omitted or "message") with role user
            if role == "user" && (type_val.is_empty() || type_val == "message") {
                n += 1;
            }
        }
    }
    n.max(1)
}

/// Normalize reasoning effort value.
pub fn normalize_grok_cli_effort(value: Option<&Json>) -> &'static str {
    match value {
        Some(Json::String(s)) => {
            let effort = s.trim().to_lowercase();
            match effort.as_str() {
                "max" => "xhigh",
                "low" | "medium" | "high" | "xhigh" => {
                    EFFORT_LEVELS.iter().find(|&&e| e == &effort).copied().unwrap_or("high")
                }
                _ => "high",
            }
        }
        _ => "high",
    }
}

/// Test helper — check if model supports grok CLI reasoning effort.
pub fn supports_grok_cli_reasoning_effort(model: Option<&str>) -> bool {
    match model {
        Some(m) => regex::Regex::new(r"^grok-4\.5(?:$|-)")
            .ok()
            .and_then(|re| re.is_match(m).then_some(true))
            .unwrap_or(false),
        None => false,
    }
}

/// Grok CLI Executor — OpenAI Responses API on cli-chat-proxy.grok.com
/// Auth: OAuth device-code access token (xai-grok-cli).
pub struct GrokCliExecutor {
    provider: String,
    config: Json,
}

impl GrokCliExecutor {
    pub fn new() -> Self {
        let provider = "grok-cli".to_string();
        let config = nr_data::provider("grok-cli")
            .cloned()
            .unwrap_or_else(|| Json::Object(Default::default()));
        Self { provider, config }
    }
}

impl Default for GrokCliExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BaseHooks for GrokCliExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn config(&self) -> &Json {
        &self.config
    }

    fn build_url(&self, _model: &str, _stream: bool, url_index: usize, _creds: &Credentials) -> Result<String> {
        Ok(nr_exec_base::base::default_build_url(self, url_index, _creds))
    }

    fn build_headers(
        &self,
        creds: &Credentials,
        stream: bool,
        _url: &str,
        _model: &str,
        _body: &Json,
    ) -> Result<HeaderList> {
        let mut headers = nr_exec_base::base::default_build_headers(self, creds, stream);

        // Static fingerprint from registry
        if let Some(static_headers) = self.config.get("headers").and_then(Json::as_object) {
            for (k, v) in static_headers {
                if let Some(v_str) = v.as_str() {
                    if !headers.contains_key(k) {
                        headers.insert(k.clone(), v_str.to_string());
                    }
                }
            }
        }

        let client_id = nr_data::export("open-sse/config/grokCli", "GROK_CLI_CLIENT_IDENTIFIER")
            .and_then(Json::as_str)
            .unwrap_or("grok-shell");
        let version = nr_data::export("open-sse/config/grokCli", "GROK_CLI_VERSION")
            .and_then(Json::as_str)
            .unwrap_or("1.0.44");

        headers.insert("x-grok-client-identifier".into(), client_id.to_string());
        headers.insert("x-grok-client-version".into(), version.to_string());

        // Session IDs would be set in transformRequest, but for now set defaults
        let session_id = Uuid::new_v4().to_string();
        let req_id = Uuid::new_v4().to_string();
        headers.insert("x-grok-session-id".into(), session_id.clone());
        headers.insert("x-grok-conv-id".into(), session_id);
        headers.insert("x-grok-req-id".into(), req_id);
        headers.insert("x-grok-turn-idx".into(), "1".to_string());

        // Identity: mapTokens stores email top-level AND in providerSpecificData
        let email = creds.psd_str("email").or_else(|| creds.str("email"));
        let user_id = creds.psd_str("userId").or_else(|| creds.psd_str("providerUserId"));

        if let Some(e) = email {
            headers.insert("x-email".into(), e.to_string());
        }
        if let Some(u) = user_id {
            headers.insert("x-userid".into(), u.to_string());
        }

        Ok(headers)
    }

    fn transform_request(&self, _model: &str, body: &Json, _stream: bool, _creds: &Credentials) -> Result<Json> {
        Ok(body.clone())
    }
}

#[async_trait]
impl nr_core::Executor for GrokCliExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        nr_exec_base::base::base_execute(self, req).await
    }
}

/// Executor for the canonical provider id (aliases are resolved in nr-executors).
pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(GrokCliExecutor::new())
}
