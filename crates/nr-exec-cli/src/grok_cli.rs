//! Port of open-sse/executors/grok-cli.js + services/grokCliModels.js + config/grokCli.js.
//!
//! OpenAI Responses API on cli-chat-proxy.grok.com.
//! Auth: OAuth device-code access token (xai-grok-cli).
//!
//! shortcut: TLS fingerprinting (wreq-js) not implemented — uses standard reqwest.
//! This means Cloudflare challenges without a cf_clearance cookie may fail.
//! When to upgrade: wrap HTTP client with TLS adapter for Chrome/Safari fingerprint.

use async_trait::async_trait;
use nr_core::http::HeaderList;
use nr_core::{Credentials, ExecRequest, ExecResult, Json, Result, Executor};
use nr_exec_base::base::BaseHooks;
use std::sync::Arc;
use uuid::Uuid;

const EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh"];

/// Count user turns in a Responses `input` array (≈ user messages).
fn count_user_turns(input: Option<&Json>) -> usize {
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
fn normalize_effort(value: Option<&Json>) -> &'static str {
    match value {
        Some(Json::String(s)) => {
            let effort = s.trim().to_lowercase();
            match effort.as_str() {
                "max" => "xhigh",
                _ if EFFORT_LEVELS.contains(&effort.as_str()) => EFFORT_LEVELS
                    .iter()
                    .find(|&&e| e == &effort)
                    .copied()
                    .unwrap_or("high"),
                _ => "high",
            }
        }
        _ => "high",
    }
}

/// Check if model supports grok CLI reasoning effort (grok-4.5+).
fn supports_reasoning_effort(model: &str) -> bool {
    model.starts_with("grok-4.5") && (model.len() == 8 || model.as_bytes()[8] == b'-')
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

    fn build_url(&self, _model: &str, _stream: bool, url_index: usize, creds: &Credentials) -> Result<String> {
        Ok(nr_exec_base::base::default_build_url(self, url_index, creds))
    }

    fn build_headers(
        &self,
        creds: &Credentials,
        stream: bool,
        _url: &str,
        model: &str,
        body: &Json,
    ) -> Result<HeaderList> {
        let mut headers = nr_exec_base::base::default_build_headers(self, creds, stream);

        let client_id = nr_data::export("open-sse/config/grokCli", "GROK_CLI_CLIENT_IDENTIFIER")
            .and_then(Json::as_str)
            .unwrap_or("grok-shell");
        let version = nr_data::export("open-sse/config/grokCli", "GROK_CLI_VERSION")
            .and_then(Json::as_str)
            .unwrap_or("1.0.44");

        headers.insert("x-grok-client-identifier".into(), client_id.to_string());
        headers.insert("x-grok-client-version".into(), version.to_string());

        // Session ID: derive from credentials.connectionId or body.session_id, else random
        let session_id = creds
            .connection_id()
            .or_else(|| body.get("session_id").and_then(Json::as_str))
            .unwrap_or("")
            .to_string();
        let session_id = if session_id.is_empty() {
            Uuid::new_v4().to_string()
        } else {
            session_id
        };

        let req_id = Uuid::new_v4().to_string();
        let turn_idx = count_user_turns(body.get("input"));

        headers.insert("x-grok-session-id".into(), session_id.clone());
        headers.insert("x-grok-conv-id".into(), session_id);
        headers.insert("x-grok-req-id".into(), req_id);
        headers.insert("x-grok-turn-idx".into(), turn_idx.to_string());

        // Agent/model overrides (optional)
        if let Some(agent_id) = creds.psd_str("agentId").or_else(|| creds.psd_str("deviceId")) {
            headers.insert("x-grok-agent-id".into(), agent_id.to_string());
        }
        if !model.is_empty() {
            headers.insert("x-grok-model-override".into(), model.to_string());
        }

        // Identity: email and userId from providerSpecificData or top-level
        if let Some(email) = creds.psd_str("email").or_else(|| creds.str("email")) {
            headers.insert("x-email".into(), email.to_string());
        }
        if let Some(user_id) = creds
            .psd_str("userId")
            .or_else(|| creds.psd_str("providerUserId"))
            .or_else(|| creds.str("userId"))
        {
            headers.insert("x-userid".into(), user_id.to_string());
        }

        Ok(headers)
    }

    fn transform_request(&self, model: &str, body: &Json, _stream: bool, _creds: &Credentials) -> Result<Json> {
        let mut b = body.clone();

        // Ensure input array exists (soft fallback from messages)
        if !b.get("input").and_then(Json::as_array).is_some() {
            if let Some(messages) = b.get("messages").and_then(Json::as_array) {
                if !messages.is_empty() {
                    let input: Vec<Json> = messages
                        .iter()
                        .filter_map(|m| {
                            m.as_object().map(|obj| {
                                let mut item = serde_json::Map::new();
                                item.insert("type".into(), Json::String("message".into()));
                                if let Some(role) = obj.get("role").and_then(Json::as_str) {
                                    item.insert("role".into(), Json::String(role.into()));
                                }
                                if let Some(content) = obj.get("content") {
                                    let content_str = match content {
                                        Json::String(s) => s.clone(),
                                        _ => serde_json::to_string(content).unwrap_or_default(),
                                    };
                                    item.insert("content".into(), Json::String(content_str));
                                }
                                Json::Object(item)
                            })
                        })
                        .collect();
                    if !input.is_empty() {
                        b["input"] = Json::Array(input);
                        b.as_object_mut().map(|o| o.shift_remove("messages"));
                    }
                } else {
                    b["input"] = Json::Array(vec![Json::Object({
                        let mut m = serde_json::Map::new();
                        m.insert("type".into(), Json::String("message".into()));
                        m.insert("role".into(), Json::String("user".into()));
                        m.insert("content".into(), Json::String("...".into()));
                        m
                    })]);
                }
            } else {
                b["input"] = Json::Array(vec![Json::Object({
                    let mut m = serde_json::Map::new();
                    m.insert("type".into(), Json::String("message".into()));
                    m.insert("role".into(), Json::String("user".into()));
                    m.insert("content".into(), Json::String("...".into()));
                    m
                })]);
            }
        }

        // Normalize tools: flatten function shape and keep hosted tools
        if let Some(tools_arr) = b.get_mut("tools").and_then(|t| t.as_array_mut()) {
            let mut normalized = Vec::new();
            for tool in tools_arr.iter() {
                if let Some(obj) = tool.as_object() {
                    let tool_type = obj.get("type").and_then(Json::as_str).unwrap_or("");

                    // Keep hosted tools as-is
                    if matches!(
                        tool_type,
                        "web_search" | "x_search" | "web_search_preview" | "file_search"
                            | "image_generation" | "code_interpreter" | "mcp" | "local_shell"
                    ) {
                        normalized.push(tool.clone());
                        continue;
                    }

                    // Extract function schema
                    let fn_obj = if tool_type == "function" || tool_type.is_empty() {
                        obj.get("function").and_then(Json::as_object)
                    } else {
                        Some(obj)
                    };

                    if let Some(fn_obj) = fn_obj {
                        let name = fn_obj
                            .get("name")
                            .and_then(Json::as_str)
                            .map(|s| s.trim())
                            .filter(|s| !s.is_empty());

                        if let Some(name) = name {
                            let mut flat = serde_json::Map::new();
                            flat.insert("type".into(), Json::String("function".into()));
                            flat.insert(
                                "name".into(),
                                Json::String(name.chars().take(128).collect()),
                            );
                            if let Some(desc) = fn_obj.get("description").and_then(Json::as_str) {
                                flat.insert("description".into(), Json::String(desc.into()));
                            }
                            if let Some(params) = fn_obj.get("parameters") {
                                flat.insert("parameters".into(), params.clone());
                            } else {
                                let mut empty_params = serde_json::Map::new();
                                empty_params.insert("type".into(), Json::String("object".into()));
                                empty_params.insert("properties".into(), Json::Object(serde_json::Map::new()));
                                flat.insert("parameters".into(), Json::Object(empty_params));
                            }
                            normalized.push(Json::Object(flat));
                        }
                    }
                }
            }
            if normalized.is_empty() {
                b.as_object_mut().map(|o| o.shift_remove("tools"));
                b.as_object_mut().map(|o| o.shift_remove("tool_choice"));
            } else {
                *tools_arr = normalized;
            }
        }

        // Set streaming and store flags
        b["stream"] = Json::Bool(true);
        b["store"] = Json::Bool(false);

        // Resolve model ID (strip effort suffix if present)
        let mut resolved_model = model.to_string();
        for level in EFFORT_LEVELS {
            if let Some(base) = model.strip_suffix(&format!("-{}", level)) {
                resolved_model = base.to_string();
                break;
            }
        }
        if !resolved_model.is_empty() {
            b["model"] = Json::String(resolved_model.clone());
        }

        // Normalize reasoning effort
        let supports_effort = supports_reasoning_effort(&resolved_model);
        if !b.get("reasoning").and_then(Json::as_object).is_some() {
            let mut reasoning = serde_json::Map::new();
            reasoning.insert("summary".into(), Json::String("concise".into()));
            if supports_effort {
                reasoning.insert(
                    "effort".into(),
                    Json::String(normalize_effort(b.get("reasoning_effort")).into()),
                );
            }
            b["reasoning"] = Json::Object(reasoning);
        } else {
            // Collect effort value before mutable borrow
            let effort_val = b
                .get("reasoning_effort")
                .cloned()
                .or_else(|| b.get("reasoning").and_then(|r| r.get("effort")).cloned());

            if let Some(obj) = b.get_mut("reasoning").and_then(|r| r.as_object_mut()) {
                if supports_effort {
                    obj.insert(
                        "effort".into(),
                        Json::String(normalize_effort(effort_val.as_ref()).into()),
                    );
                } else {
                    obj.shift_remove("effort");
                }
                if !obj.contains_key("summary") {
                    obj.insert("summary".into(), Json::String("concise".into()));
                }
            }
        }
        b.as_object_mut().map(|o| o.shift_remove("reasoning_effort"));

        // Include encrypted reasoning in response
        if let Some(reasoning) = b.get("reasoning").and_then(Json::as_object) {
            if reasoning.get("effort").and_then(Json::as_str).unwrap_or("") != "none" {
                let include = b
                    .get_mut("include")
                    .and_then(|i| i.as_array_mut())
                    .unwrap_or_else(|| {
                        b["include"] = Json::Array(Vec::new());
                        b.get_mut("include").unwrap().as_array_mut().unwrap()
                    });
                if !include.iter().any(|i| i.as_str() == Some("reasoning.encrypted_content")) {
                    include.push(Json::String("reasoning.encrypted_content".into()));
                }
            }
        }

        Ok(b)
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
