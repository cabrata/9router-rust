//! Port of open-sse/executors/default.js — the executor for every
//! OpenAI-compatible provider without a specialized executor.
//!
//! Includes auth descriptors, header hooks, json_schema fallback,
//! Nous attribution, and refresh grant logic.

use std::sync::{Arc, LazyLock};
use std::collections::HashMap;

use async_trait::async_trait;
use nr_core::{ExecRequest, ExecResult, Executor, Json, Result, Error};
use nr_core::http::HeaderList;
use uuid::Uuid;

use crate::base::{base_execute, BaseHooks};

static EMPTY: LazyLock<Json> = LazyLock::new(|| Json::Object(Default::default()));

// Auth descriptor: how to apply API key or OAuth token to headers.
#[derive(Clone, Debug)]
pub struct AuthDescriptor {
    pub combined: bool,
    pub header: String,
    pub scheme: String,
    pub api_key: Option<Box<AuthDescriptor>>,
    pub oauth: Option<Box<AuthDescriptor>>,
    pub anthropic_version: bool,
    pub hooks: Vec<String>,
}

impl AuthDescriptor {
    fn bearer() -> Self {
        Self {
            combined: true,
            header: "Authorization".into(),
            scheme: "bearer".into(),
            api_key: None,
            oauth: None,
            anthropic_version: false,
            hooks: Vec::new(),
        }
    }

    fn x_api_key() -> Self {
        Self {
            combined: true,
            header: "x-api-key".into(),
            scheme: "raw".into(),
            api_key: None,
            oauth: None,
            anthropic_version: false,
            hooks: Vec::new(),
        }
    }
}

pub struct DefaultExecutor {
    provider: String,
    config: &'static Json,
}

impl DefaultExecutor {
    /// JS `new DefaultExecutor(provider)`: config is `PROVIDERS[provider] || PROVIDERS.openai`.
    pub fn new(provider: &str) -> Self {
        let config = nr_data::provider(provider).or_else(|| nr_data::provider("openai")).unwrap_or(&EMPTY);
        Self { provider: provider.to_string(), config }
    }

    /// Resolve auth descriptor from registry or fallback.
    fn resolve_auth_descriptor(&self) -> AuthDescriptor {
        if self.provider.starts_with("anthropic-compatible-") {
            return AuthDescriptor {
                combined: false,
                header: "".into(),
                scheme: "".into(),
                api_key: Some(Box::new(AuthDescriptor {
                    combined: false,
                    header: "x-api-key".into(),
                    scheme: "raw".into(),
                    api_key: None,
                    oauth: None,
                    anthropic_version: false,
                    hooks: Vec::new(),
                })),
                oauth: Some(Box::new(AuthDescriptor {
                    combined: false,
                    header: "Authorization".into(),
                    scheme: "bearer".into(),
                    api_key: None,
                    oauth: None,
                    anthropic_version: false,
                    hooks: Vec::new(),
                })),
                anthropic_version: true,
                hooks: Vec::new(),
            };
        }
        if self.config.get("format").and_then(Json::as_str) == Some("claude") {
            let mut x_api_key = AuthDescriptor::x_api_key();
            x_api_key.anthropic_version = true;
            return x_api_key;
        }
        AuthDescriptor::bearer()
    }

    /// Apply json_schema → json_object fallback for openai-compatible providers.
    fn apply_json_schema_fallback(&self, body: &Json) -> Json {
        if !self.provider.starts_with("openai-compatible-") {
            return body.clone();
        }
        let rf = body.get("response_format")
            .and_then(|f| f.as_object())
            .and_then(|f| {
                if f.get("type").and_then(Json::as_str) == Some("json_schema") {
                    f.get("json_schema").and_then(|js| js.get("schema"))
                } else {
                    None
                }
            });

        if rf.is_none() {
            return body.clone();
        }

        let schema = rf.unwrap();
        let schema_json = serde_json::to_string_pretty(schema).unwrap_or_default();
        let prompt = format!(
            "You must respond with valid JSON that strictly follows this JSON schema:\n```json\n{}\n```\nRespond ONLY with the JSON object, no other text.",
            schema_json
        );

        let mut messages = body.get("messages")
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default();

        let mut found_system = false;
        for msg in &mut messages {
            if msg.get("role").and_then(Json::as_str) == Some("system") {
                found_system = true;
                if let Some(obj) = msg.as_object_mut() {
                    if let Some(content) = obj.get_mut("content") {
                        match content {
                            Json::String(s) => {
                                *s = format!("{}\n\n{}", s, prompt);
                            }
                            Json::Array(arr) => {
                                arr.push(serde_json::json!({"type": "text", "text": format!("\n\n{}", prompt)}));
                            }
                            _ => {}
                        }
                    }
                }
                break;
            }
        }

        if !found_system {
            messages.insert(0, serde_json::json!({"role": "system", "content": prompt}));
        }

        let mut result = body.clone();
        if let Some(obj) = result.as_object_mut() {
            obj.insert("messages".to_string(), Json::Array(messages));
            obj.insert("response_format".to_string(), serde_json::json!({"type": "json_object"}));
        }
        result
    }

    /// Apply Nous attribution tags for openai-compatible providers.
    fn apply_nous_attribution(&self, model: &str, body: &Json, _stream: bool, creds: &nr_core::Credentials) -> Json {
        if !self.provider.starts_with("openai-compatible-")
            || creds.api_key().is_none()
            || !matches!(body, Json::Object(_)) {
            return body.clone();
        }

        // Read Nous configuration from data
        let nous_config = match nr_data::export("open-sse/config/appConstants", "NOUS_ATTRIBUTION") {
            Some(cfg) => cfg,
            None => return body.clone(),
        };

        let nous_hostname = nous_config.get("hostname").and_then(Json::as_str).unwrap_or("inference-api.nousresearch.com");
        let nous_path = nous_config.get("chatCompletionsPath").and_then(Json::as_str).unwrap_or("/v1/chat/completions");
        let tags_fallback: Vec<String> = nous_config.get("tags")
            .and_then(Json::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_else(|| vec!["product=9router".to_string(), "client=9router".to_string(), "user=9router".to_string()]);

        // Try to parse URL to check if it matches Nous
        let url = match self.build_url(model, false, 0, creds) {
            Ok(url) => url,
            Err(_) => return body.clone(),
        };

        if let Ok(parsed_url) = url::Url::parse(&url) {
            if parsed_url.host_str() != Some(nous_hostname) || parsed_url.path() != nous_path {
                return body.clone();
            }
        } else {
            return body.clone();
        }

        let mut tags: Vec<String> = body.get("tags")
            .and_then(Json::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        for fallback_tag in &tags_fallback {
            let prefix = fallback_tag.split('=').next().map(|p| format!("{}=", p)).unwrap_or_default();
            if !prefix.is_empty() {
                tags.retain(|tag| !tag.starts_with(&prefix) || tag[prefix.len()..].trim().is_empty());
            }
            if !tags.iter().any(|tag| tag.starts_with(&prefix)) && !fallback_tag[prefix.len()..].trim().is_empty() {
                tags.push(fallback_tag.clone());
            }
        }

        tags.sort();
        tags.dedup();

        let mut result = body.clone();
        if let Some(obj) = result.as_object_mut() {
            obj.insert("tags".to_string(), Json::Array(tags.into_iter().map(Json::String).collect()));
        }
        result
    }

    /// Apply header hooks for provider-specific quirks.
    fn apply_header_hooks(&self, headers: &mut HeaderList, creds: &nr_core::Credentials) {
        match self.provider.as_str() {
            "codebuddy-cn" | "codebuddy-intl" => {
                let request_id = Uuid::new_v4().to_string();
                headers.insert("X-Request-ID".into(), request_id.clone());
                headers.insert("X-Conversation-ID".into(), Uuid::new_v4().to_string());
                headers.insert("X-Conversation-Request-ID".into(), request_id.clone());
                headers.insert("X-Conversation-Message-ID".into(), request_id);
            }
            "kimi" | "kimi-coding" => {
                let device_id = creds.psd_str("deviceId").map(String::from);
                if let Some(headers_to_add) = build_kimi_headers(device_id.as_deref()) {
                    for (k, v) in headers_to_add {
                        headers.insert(k, v);
                    }
                }
            }
            "minimax-code" => {
                headers.insert("x-api-key".into(), "sk-xxx".into());
                let tz_offset = chrono::Local::now().format("%z").to_string();
                let tz_seconds = if tz_offset.len() >= 3 {
                    let sign = if &tz_offset[0..1] == "-" { -1 } else { 1 };
                    let hours: i32 = tz_offset[1..3].parse().unwrap_or(0);
                    let mins: i32 = tz_offset[3..5].parse().unwrap_or(0);
                    sign * (hours * 3600 + mins * 60)
                } else {
                    0
                };
                headers.insert("X-Mavis-Timezone-Offset".into(), tz_seconds.to_string());
                if !headers.contains_key("X-Mavis-Session-Id") {
                    headers.insert("X-Mavis-Session-Id".into(), Uuid::new_v4().to_string());
                }
            }
            "cline" | "clinepass" => {
                if let Some(token) = creds.api_key().or_else(|| creds.access_token()) {
                    if let Some(headers_to_add) = build_cline_headers(token) {
                        for (k, v) in headers_to_add {
                            headers.insert(k, v);
                        }
                    }
                }
            }
            "kilocode" => {
                if let Some(org_id) = creds.psd_str("orgId") {
                    headers.insert("X-Kilocode-OrganizationID".into(), org_id.to_string());
                }
            }
            _ => {}
        }
    }

    /// Strip first-party Claude identity headers for non-Anthropic anthropic-compatible upstreams.
    fn strip_first_party_claude_headers(&self, headers: &mut HeaderList, creds: &nr_core::Credentials) {
        if !self.provider.starts_with("anthropic-compatible-") {
            return;
        }

        let base_url = creds.psd_str("baseUrl").unwrap_or("");
        let is_official_anthropic = base_url.is_empty() || base_url.contains("api.anthropic.com");
        if is_official_anthropic {
            return;
        }

        if creds.api_key().is_some() && !headers.contains_key("Authorization") {
            if let Some(key) = creds.api_key() {
                headers.insert("Authorization".into(), format!("Bearer {}", key));
            }
        }

        headers.shift_remove("anthropic-dangerous-direct-browser-access");
        headers.shift_remove("Anthropic-Dangerous-Direct-Browser-Access");
        headers.shift_remove("x-app");
        headers.shift_remove("X-App");

        for beta_key in &["anthropic-beta", "Anthropic-Beta"] {
            if let Some(beta_val) = headers.get(*beta_key) {
                let filtered: String = beta_val
                    .split(',')
                    .map(|s| s.trim())
                    .filter(|f| !f.is_empty() && *f != "claude-code-20250219")
                    .collect::<Vec<_>>()
                    .join(",");
                if filtered.is_empty() {
                    headers.shift_remove(*beta_key);
                } else {
                    headers.insert((*beta_key).into(), filtered);
                }
            }
        }
    }

    /// Extract Claude session ID from metadata user_id field.
    fn extract_claude_session_id(&self, user_id: &str) -> Option<String> {
        if user_id.is_empty() {
            return None;
        }
        if user_id.starts_with('{') {
            if let Ok(obj) = serde_json::from_str::<serde_json::Value>(user_id) {
                if let Some(sid) = obj.get("session_id").and_then(|v| v.as_str()) {
                    let clean = sid.trim_start_matches("claude:").trim();
                    return if clean.is_empty() { None } else { Some(clean.to_string()) };
                }
            }
            return None;
        }
        let clean = user_id.trim_start_matches("claude:").trim();
        if clean.is_empty() { None } else { Some(clean.to_string()) }
    }
}

impl BaseHooks for DefaultExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn config(&self) -> &Json {
        self.config
    }

    fn build_url(&self, model: &str, stream: bool, _url_index: usize, creds: &nr_core::Credentials) -> Result<String> {
        let rt = creds.get("runtimeTransport").and_then(Json::as_object);
        if let Some(rt) = rt {
            if let Some(base_url) = rt.get("baseUrl").and_then(Json::as_str) {
                if let Some(suffix) = rt.get("urlSuffix").and_then(Json::as_str) {
                    return Ok(format!("{}{}", base_url, suffix));
                }
                return Ok(base_url.to_string());
            }
        }

        if self.provider.starts_with("openai-compatible-") {
            let base_url = creds.psd_str("baseUrl").unwrap_or("https://api.openai.com/v1");
            let normalized = base_url.trim_end_matches('/');
            let api_type = resolve_openai_compatible_api_type(&self.provider, creds.psd_str("apiType"));
            let path = if api_type == "responses" { "/responses" } else { "/chat/completions" };
            return Ok(format!("{}{}", normalized, path));
        }

        if self.provider.starts_with("anthropic-compatible-") {
            let base_url = creds.psd_str("baseUrl").unwrap_or("https://api.anthropic.com/v1");
            let normalized = base_url.trim_end_matches('/');
            return Ok(format!("{}/messages", normalized));
        }

        if self.config.get("format").and_then(Json::as_str) == Some("gemini") {
            if let Some(base_url) = self.config.get("baseUrl").and_then(Json::as_str) {
                let endpoint = if stream { "streamGenerateContent?alt=sse" } else { "generateContent" };
                return Ok(format!("{}{}:{}", base_url, model, endpoint));
            }
        }

        if let Some(suffix) = self.config.get("urlSuffix").and_then(Json::as_str) {
            if let Some(base_url) = self.config.get("baseUrl").and_then(Json::as_str) {
                return Ok(format!("{}{}", base_url, suffix));
            }
        }

        if let Some(url) = self.config.get("baseUrl").and_then(Json::as_str) {
            if url.contains("{accountId}") {
                if let Some(account_id) = creds.psd_str("accountId") {
                    return Ok(url.replace("{accountId}", account_id));
                }
                return Err(Error::BadRequest(format!("{} requires accountId in providerSpecificData", self.provider)));
            }
            return Ok(url.to_string());
        }

        Err(Error::BadRequest(format!("No baseUrl for {}", self.provider)))
    }

    fn build_headers(&self, creds: &nr_core::Credentials, stream: bool, _url: &str, model: &str, body: &Json) -> Result<HeaderList> {
        let rt = creds.get("runtimeTransport").and_then(Json::as_object);
        let mut headers = if let Some(rt) = rt {
            if let Some(rt_headers) = rt.get("headers").and_then(Json::as_object) {
                let mut h = HeaderList::new();
                for (k, v) in rt_headers {
                    if let Some(v_str) = v.as_str() {
                        h.insert(k.clone(), v_str.to_string());
                    }
                }
                h
            } else {
                HeaderList::new()
            }
        } else if let Some(config_headers) = self.config.get("headers").and_then(Json::as_object) {
            let mut h = HeaderList::new();
            for (k, v) in config_headers {
                if let Some(v_str) = v.as_str() {
                    h.insert(k.clone(), v_str.to_string());
                }
            }
            h
        } else {
            HeaderList::new()
        };

        headers.insert("Content-Type".into(), "application/json".into());

        let desc = if let Some(rt) = rt {
            rt.get("auth")
                .and_then(|_| Some(self.resolve_auth_descriptor()))
                .unwrap_or_else(|| self.resolve_auth_descriptor())
        } else {
            self.resolve_auth_descriptor()
        };

        self.apply_header_hooks(&mut headers, creds);

        apply_auth(&mut headers, &desc, creds);

        let is_claude_model = model.starts_with("claude-");
        let raw_headers = creds.get("rawHeaders").and_then(Json::as_object);
        if self.provider == "claude" || (self.provider.starts_with("anthropic-compatible-") && is_claude_model) {
            if let Some(beta) = select_anthropic_beta(model, body) {
                if let Some(existing_beta) = raw_headers.and_then(|h| h.get("anthropic-beta")).and_then(Json::as_str) {
                    let merged = merge_anthropic_beta(&beta, existing_beta);
                    headers.insert("Anthropic-Beta".into(), merged);
                } else {
                    headers.insert("Anthropic-Beta".into(), beta);
                }
            }
        } else if self.provider == "anthropic" {
            if let Some(client_beta) = raw_headers.and_then(|h| h.get("anthropic-beta")).and_then(Json::as_str) {
                if let Some(existing_beta) = headers.get("Anthropic-Beta") {
                    let merged = merge_anthropic_beta(existing_beta, client_beta);
                    headers.insert("Anthropic-Beta".into(), merged);
                }
            }
        }

        if self.provider == "claude" && !headers.contains_key("x-claude-code-session-id") {
            if let Some(token) = creds.access_token().or_else(|| creds.api_key()) {
                if token.contains("sk-ant-oat") {
                    if let Some(user_id) = body.get("metadata").and_then(|m| m.get("user_id")).and_then(Json::as_str) {
                        if let Some(sid) = self.extract_claude_session_id(user_id) {
                            headers.insert("x-claude-code-session-id".into(), sid);
                        }
                    }
                }
            }
        }

        self.strip_first_party_claude_headers(&mut headers, creds);

        if stream {
            headers.insert("Accept".into(), "text/event-stream".into());
        }

        Ok(headers)
    }

    fn transform_request(&self, model: &str, body: &Json, stream: bool, creds: &nr_core::Credentials) -> Result<Json> {
        let mut transformed = self.apply_nous_attribution(model, body, stream, creds);
        transformed = self.apply_json_schema_fallback(&transformed);

        if let Some(obj) = transformed.as_object_mut() {
            if stream != true && obj.get("stream_options").is_some() && obj.get("stream").and_then(Json::as_bool) != Some(true) {
                obj.remove("stream_options");
            }
            if self.config.get("quirks").and_then(|q| q.get("dropClientMetadata")).and_then(Json::as_bool).unwrap_or(false) {
                obj.remove("client_metadata");
            }
        }

        strip_unsupported_params(&self.provider, model, &mut transformed);
        transformed = inject_reasoning_content(&self.provider, model, transformed);

        Ok(transformed)
    }
}

#[async_trait]
impl Executor for DefaultExecutor {
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

/// Apply auth token to headers according to the descriptor.
fn apply_auth(headers: &mut HeaderList, desc: &AuthDescriptor, creds: &nr_core::Credentials) {
    if desc.combined {
        let token = creds.api_key().or_else(|| creds.access_token());
        if let Some(token) = token {
            let value = if desc.scheme == "bearer" {
                format!("Bearer {}", token)
            } else {
                token.to_string()
            };
            headers.insert(desc.header.clone(), value);
        } else {
            headers.insert(desc.header.clone(), "Bearer undefined".into());
        }
        if desc.anthropic_version {
            if !headers.contains_key("anthropic-version") {
                headers.insert(
                    "anthropic-version".into(),
                    nr_data::export("open-sse/providers/shared", "ANTHROPIC_API_VERSION")
                        .and_then(Json::as_str)
                        .unwrap_or("2023-06-01")
                        .to_string(),
                );
            }
        }
    } else {
        if let Some(key) = creds.api_key() {
            if let Some(api_key_desc) = &desc.api_key {
                let value = if api_key_desc.scheme == "bearer" {
                    format!("Bearer {}", key)
                } else {
                    key.to_string()
                };
                headers.insert(api_key_desc.header.clone(), value);
            }
        } else if let Some(token) = creds.access_token() {
            if let Some(oauth_desc) = &desc.oauth {
                let value = if oauth_desc.scheme == "bearer" {
                    format!("Bearer {}", token)
                } else {
                    token.to_string()
                };
                headers.insert(oauth_desc.header.clone(), value);
            }
        }
        if desc.anthropic_version {
            if !headers.contains_key("anthropic-version") {
                headers.insert(
                    "anthropic-version".into(),
                    nr_data::export("open-sse/providers/shared", "ANTHROPIC_API_VERSION")
                        .and_then(Json::as_str)
                        .unwrap_or("2023-06-01")
                        .to_string(),
                );
            }
        }
    }
}

/// Build Kimi-specific headers from device ID.
fn build_kimi_headers(device_id: Option<&str>) -> Option<HashMap<String, String>> {
    let mut headers = HashMap::new();
    headers.insert("X-Msh-Platform".to_string(), "9router".to_string());
    headers.insert(
        "X-Msh-Version".to_string(),
        nr_data::export("open-sse/config/appConstants", "APP_VERSION")
            .and_then(Json::as_str)
            .unwrap_or("0.0.0")
            .to_string(),
    );

    let device_name = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown".to_string());
    headers.insert("X-Msh-Device-Name".to_string(), device_name);

    let device_model = format!("{} {}", std::env::consts::OS, std::env::consts::ARCH);
    headers.insert("X-Msh-Device-Model".to_string(), device_model);

    let resolved_id = if let Some(id) = device_id {
        id.trim().to_string()
    } else {
        format!("kimi-{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis())
    };
    headers.insert("X-Msh-Device-Id".to_string(), resolved_id);

    Some(headers)
}

/// Build Cline-specific headers from token.
fn build_cline_headers(token: &str) -> Option<HashMap<String, String>> {
    let mut headers = HashMap::new();
    headers.insert("HTTP-Referer".to_string(), "https://cline.bot".to_string());
    headers.insert("X-Title".to_string(), "Cline".to_string());

    let version = nr_data::export("open-sse/config/appConstants", "APP_VERSION")
        .and_then(Json::as_str)
        .unwrap_or("0.0.0");
    headers.insert("User-Agent".to_string(), format!("9Router/{}", version));

    headers.insert("X-PLATFORM".to_string(), std::env::consts::OS.to_string());
    headers.insert("X-PLATFORM-VERSION".to_string(), "unknown".to_string());
    headers.insert("X-CLIENT-TYPE".to_string(), "9router".to_string());
    headers.insert("X-CLIENT-VERSION".to_string(), version.to_string());
    headers.insert("X-CORE-VERSION".to_string(), version.to_string());
    headers.insert("X-IS-MULTIROOT".to_string(), "false".to_string());

    let access_token = get_cline_access_token(token);
    if !access_token.is_empty() {
        headers.insert("Authorization".to_string(), format!("Bearer {}", access_token));
    }

    Some(headers)
}

/// Get Cline access token, prefixing WorkOS JWTs.
fn get_cline_access_token(token: &str) -> String {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.to_lowercase().starts_with("workos:") {
        return trimmed.to_string();
    }
    let is_workos_jwt = trimmed.starts_with("eyJ");
    if is_workos_jwt {
        format!("workos:{}", trimmed)
    } else {
        trimmed.to_string()
    }
}

/// Select anthropic-beta flags for a model (`shared.js::selectAnthropicBeta`).
fn select_anthropic_beta(model: &str, body: &Json) -> Option<String> {
    let beta = nr_data::select_anthropic_beta(model, Some(body));
    (!beta.is_empty()).then_some(beta)
}

/// Merge client-provided anthropic-beta with provider defaults
/// (`shared.js::mergeAnthropicBeta` — first-seen order, deduped).
fn merge_anthropic_beta(provider_beta: &str, client_beta: &str) -> String {
    nr_data::merge_anthropic_beta(&[provider_beta, client_beta])
}

/// Resolve OpenAI-compatible API type (chat or responses endpoint).
fn resolve_openai_compatible_api_type(provider: &str, api_type: Option<&str>) -> &'static str {
    match api_type {
        Some("chat") => "chat",
        Some("responses") => "responses",
        _ if provider.contains("responses") => "responses",
        _ => "chat",
    }
}

/// Strip request params a given provider/model rejects (HTTP 400).
/// Port of translator/concerns/paramSupport.js::stripUnsupportedParams.
fn strip_unsupported_params(provider: &str, model: &str, body: &mut Json) {
    if !matches!(body, Json::Object(_)) {
        return;
    }

    // All Claude models: temperature deprecated/rejected upstream
    if model.to_lowercase().contains("claude") {
        if let Some(obj) = body.as_object_mut() {
            obj.remove("temperature");
        }
    }

    // GitHub Copilot gpt-5.4: temperature unsupported
    if provider == "github" && model.to_lowercase().contains("gpt-5.4") {
        if let Some(obj) = body.as_object_mut() {
            obj.remove("temperature");
        }
    }

    // GitHub Copilot Claude (except opus/sonnet 4.6): thinking + reasoning_effort rejected.
    // The provider gate must bind the whole condition: outside `github` neither
    // field is rejected, and dropping them there silenced reasoning everywhere.
    if provider == "github" {
        let m = model.to_lowercase();
        if m.contains("claude") && !m.contains("4.6") {
            if let Some(obj) = body.as_object_mut() {
                obj.remove("thinking");
                obj.remove("reasoning_effort");
            }
        }
    }

    // Cloudflare Workers AI: flatten content array to plain string
    if provider == "cloudflare-ai" {
        if let Some(obj) = body.as_object_mut() {
            if let Some(Json::Array(messages)) = obj.get_mut("messages") {
                for msg in messages {
                    if let Some(obj) = msg.as_object_mut() {
                        if let Some(Json::Array(content)) = obj.get_mut("content") {
                            let text: String = content.iter()
                                .filter_map(|c| {
                                    c.as_object().and_then(|co| {
                                        if co.get("type").and_then(Json::as_str) == Some("text") {
                                            co.get("text").and_then(Json::as_str).map(|s| s.to_string())
                                        } else {
                                            None
                                        }
                                    })
                                })
                                .collect::<Vec<_>>()
                                .join("");
                            obj.insert("content".to_string(), Json::String(text));
                        }
                    }
                }
            }
        }
    }

    // Groq, Mistral, Cerebras: drop reasoning fields from assistant messages
    if ["groq", "mistral", "cerebras"].contains(&provider) {
        if let Some(obj) = body.as_object_mut() {
            if let Some(Json::Array(messages)) = obj.get_mut("messages") {
                for msg in messages {
                    if let Some(obj) = msg.as_object_mut() {
                        if obj.get("role").and_then(Json::as_str) == Some("assistant") {
                            obj.remove("reasoning_content");
                            obj.remove("reasoning");
                            obj.remove("reasoning_details");
                        }
                    }
                }
            }
        }
    }

    // NVIDIA NIM: rename reasoning → reasoning_content
    if provider == "nvidia" {
        if let Some(obj) = body.as_object_mut() {
            if let Some(Json::Array(messages)) = obj.get_mut("messages") {
                for msg in messages {
                    if let Some(obj) = msg.as_object_mut() {
                        if obj.get("role").and_then(Json::as_str) == Some("assistant") {
                            if let Some(val) = obj.remove("reasoning") {
                                if obj.get("reasoning_content").is_none() || obj.get("reasoning_content") == Some(&Json::Null) {
                                    obj.insert("reasoning_content".to_string(), val);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // OpenCode Go deepseek: drop response_format
    if provider == "opencode-go" && model.to_lowercase().contains("deepseek") {
        if let Some(obj) = body.as_object_mut() {
            obj.remove("response_format");
        }
    }

    // OpenAI: rename max_tokens → max_completion_tokens
    if provider == "openai" {
        if let Some(obj) = body.as_object_mut() {
            if let Some(val) = obj.remove("max_tokens") {
                if obj.get("max_completion_tokens").is_none() {
                    obj.insert("max_completion_tokens".to_string(), val);
                }
            }
        }
    }
}

/// Inject reasoning_content placeholder for providers that require it.
/// Port of open-sse/utils/reasoningContentInjector.js::injectReasoningContent.
fn inject_reasoning_content(provider: &str, model: &str, mut body: Json) -> Json {
    if !matches!(body, Json::Object(_)) {
        return body;
    }

    const PLACEHOLDER: &str = " ";

    let should_inject = |provider: &str, model: &str| -> bool {
        // Kimi models: inject in tool calls only
        if model.to_lowercase().starts_with("kimi-") {
            return true;
        }
        // DeepSeek models: inject in all assistant messages
        if model.to_lowercase().contains("deepseek") {
            return true;
        }
        // Check provider-level rules from registry
        ["deepseek", "kimi"].contains(&provider)
    };

    if !should_inject(provider, model) {
        return body;
    }

    // Inject reasoning_content on assistant messages
    if let Some(obj) = body.as_object_mut() {
        if let Some(Json::Array(messages)) = obj.get_mut("messages") {
            for msg in messages {
                if let Some(msg_obj) = msg.as_object_mut() {
                    if msg_obj.get("role").and_then(Json::as_str) == Some("assistant") {
                        let rc = msg_obj.get("reasoning_content");
                        let should_add = match rc {
                            Some(Json::String(s)) if s.is_empty() => true,
                            Some(Json::Null) => true,
                            None => {
                                // For kimi: only inject if there are tool_calls
                                if model.to_lowercase().starts_with("kimi-") {
                                    msg_obj.get("tool_calls").and_then(Json::as_array).map(|a| !a.is_empty()).unwrap_or(false)
                                } else {
                                    true
                                }
                            }
                            _ => false,
                        };
                        if should_add {
                            msg_obj.insert("reasoning_content".to_string(), Json::String(PLACEHOLDER.to_string()));
                        }
                    }
                }
            }
        }
    }

    body
}

/// Shared constructor used by `nr-executors::get_executor` and by stubs.
pub fn create(provider: &str) -> Arc<dyn Executor> {
    Arc::new(DefaultExecutor::new(provider))
}
