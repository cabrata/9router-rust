//! Port of open-sse/executors/opencode.js.
//!
//! OpenCode v2 free-tier with session management, responses API support, and TLS fingerprinting.

use std::sync::Arc;
use std::sync::Mutex;
use std::collections::HashMap;

use async_trait::async_trait;
use nr_core::{ExecRequest, ExecResult, Executor, Json, Result, Error, Credentials};
use nr_exec_base::base::{base_execute, BaseHooks};
use std::sync::LazyLock;

const OPENCODE_UA: &str = "opencode/latest/2.0.26/cli";
const OPENCODE_CLIENT: &str = "cli";
const SESSION_HEADER: &str = "x-opencode-session";
const SESSION_FIELD: &str = "_opencodeSession";
const MAX_SESSION_LENGTH: usize = 256;
const BASE62_CHARS: &str = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

// Models served by /inference/v1/responses; every other model stays on /chat/completions.
const RESPONSES_MODELS: &[&str] = &[
    "muse-spark-1.2-contributor-free",
    "muse-spark-1.3-contributor-free",
];
const MESSAGES_MODELS: &[&str] = &["union-alpha"];

static EMPTY: LazyLock<Json> = LazyLock::new(|| Json::Object(Default::default()));

// Session cache for stable session IDs per identity
static SESSION_CACHE: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Generate a session ID with timestamp-based prefix and random suffix.
fn generate_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    // Session prefix = 12 hex of !(timeMs * 0x1000 + counter)
    let session_prefix_mask = (1u64 << 48) - 1;
    let descending_time = (!(now_ms * 0x1000 + 1) & session_prefix_mask) as u64;
    let hex_time = format!("{:012x}", descending_time);

    // Random suffix
    let mut random_part = String::new();
    use rand::Rng;
    let mut rng = rand::thread_rng();
    for _ in 0..14 {
        let idx = rng.gen_range(0..62);
        random_part.push(BASE62_CHARS.chars().nth(idx).unwrap());
    }

    format!("ses_{}{}", hex_time, random_part)
}

/// `OPENCODE_SESSION_RE`: a session id the upstream already accepts.
fn is_opencode_session(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 4 + 12 + 14 {
        return false;
    }
    if !value.starts_with("ses_") {
        return false;
    }
    let hex = &value[4..16];
    if !hex.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
        return false;
    }
    value[16..].bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Cache of translated sessions, keyed by `{clientTool}\0{incoming id}`.
static TRANSLATED_SESSIONS: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const MAX_TRANSLATED_SESSIONS: usize = 5000;

/// `translateSessionId`: keep a conversation on one valid upstream session id.
///
/// Upstream decodes the prefix timestamp and refuses future or stale ids, so a
/// fresh `Date.now()`-based id is minted per conversation rather than hashing.
fn translate_session_id(session_id: &str, client_tool: &str) -> String {
    let trimmed = session_id.trim();
    if is_opencode_session(trimmed) {
        return trimmed.to_string();
    }
    let key = format!(
        "{}\0{}",
        if client_tool.is_empty() { "generic" } else { client_tool },
        session_id
    );

    let mut cache = TRANSLATED_SESSIONS.lock().unwrap();
    if let Some(existing) = cache.get(&key) {
        return existing.clone();
    }
    let translated = generate_session_id();
    if cache.len() >= MAX_TRANSLATED_SESSIONS {
        if let Some(oldest) = cache.keys().next().cloned() {
            cache.remove(&oldest);
        }
    }
    cache.insert(key, translated.clone());
    translated
}

/// `bodyHasSessionHints`: does this body carry a client-side conversation id?
fn body_has_session_hints(body: &Json) -> bool {
    let non_empty = |value: Option<&Json>| {
        value
            .and_then(Json::as_str)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    };
    if non_empty(body.get("session_id"))
        || non_empty(body.get("conversation_id"))
        || non_empty(body.get("prompt_cache_key"))
        || body
            .get("metadata")
            .and_then(|m| m.get("user_id"))
            .and_then(Json::as_str)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    {
        return true;
    }
    if body
        .get("request")
        .and_then(|r| r.get("sessionId"))
        .map(|v| !v.is_null() && v.to_string() != "\"\"")
        .unwrap_or(false)
    {
        return true;
    }
    // A long assistant turn means the client is continuing an existing
    // conversation, even without an explicit id.
    let messages = body
        .get("messages")
        .and_then(Json::as_array)
        .or_else(|| body.get("input").and_then(Json::as_array));
    if let Some(messages) = messages {
        let mut assistant_text = String::new();
        for msg in messages {
            if msg.get("role").and_then(Json::as_str) != Some("assistant") {
                continue;
            }
            match msg.get("content") {
                Some(Json::String(s)) => assistant_text.push_str(s),
                Some(Json::Array(parts)) => {
                    for part in parts {
                        if let Some(text) = part
                            .get("text")
                            .and_then(Json::as_str)
                            .or_else(|| part.get("output").and_then(Json::as_str))
                        {
                            assistant_text.push_str(text);
                        }
                    }
                }
                _ => {}
            }
            if assistant_text.len() >= 50 {
                return true;
            }
        }
    }
    false
}

/// Normalize session value.
fn normalize_session(value: &str) -> Option<String> {
    let normalized = value.trim();
    if normalized.is_empty() || normalized.len() > MAX_SESSION_LENGTH {
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

/// Get or create stable session for an identity.
fn stable_session_id(credentials: &Credentials) -> String {
    let key = if let Some(conn_id) = credentials.get("connectionId").and_then(Json::as_str) {
        format!("opencode:conn:{}", &conn_id[..conn_id.len().min(128)])
    } else {
        "opencode:default".to_string()
    };

    let mut cache = SESSION_CACHE.lock().unwrap();
    if let Some(sid) = cache.get(&key) {
        return sid.clone();
    }

    let sid = generate_session_id();
    cache.insert(key, sid.clone());
    sid
}

/// Resolve OpenCode session, preferring native > translated > stable.
fn resolve_opencode_session(
    body: &Json,
    credentials: &Credentials,
    provider_session_id: Option<&str>,
    client_tool: Option<&str>,
) -> String {
    let client_tool = client_tool.unwrap_or("generic");

    // 1. Try native session from headers
    if let Some(native) = native_session(credentials) {
        return native;
    }

    // 2. Translate an incoming provider session id once.
    let incoming = provider_session_id.and_then(normalize_session);
    if let Some(hinted) = incoming {
        return translate_session_id(&hinted, client_tool);
    }

    // 3. Fall back to the session manager for conversation-stable identity.
    let connection_id = credentials.get("connectionId").and_then(Json::as_str);
    if connection_id.is_some() || body_has_session_hints(body) {
        let headers = credentials
            .get("rawHeaders")
            .and_then(Json::as_object)
            .cloned();
        let via_manager = nr_common::session_manager::resolve_session_id(
            headers.as_ref(),
            body,
            connection_id,
            None,
            "opencode",
        );
        if !via_manager.is_empty() {
            return translate_session_id(&via_manager, client_tool);
        }
    }

    // 4. Use stable session for connection
    stable_session_id(credentials)
}

struct OpencodeExecutor {
    config: &'static Json,
}

/// JS `baseModelId`: strip thinking suffix "(level)".
fn base_model_id(model: &str) -> &str {
    let trimmed = model.trim();
    let base = trimmed.find('(').map(|i| &trimmed[..i]).unwrap_or(trimmed);
    base.trim()
}

fn is_responses_model(model: &str) -> bool {
    let base = base_model_id(model);
    RESPONSES_MODELS.contains(&base) || nr_data::model_helpers::is_muse_spark_model(Some(base))
}

fn is_messages_model(model: &str) -> bool {
    MESSAGES_MODELS.contains(&base_model_id(model))
}

impl OpencodeExecutor {
    fn new() -> Self {
        let config = nr_data::provider("opencode")
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
        let session = resolve_opencode_session(body, &creds, provider_session_id, client_tool);
        creds.set(SESSION_FIELD, Json::String(session));
        creds
    }
}

impl BaseHooks for OpencodeExecutor {
    fn provider(&self) -> &str {
        "opencode"
    }

    fn config(&self) -> &Json {
        self.config
    }

    fn build_url(&self, model: &str, _stream: bool, _url_index: usize, _creds: &Credentials) -> Result<String> {
        let base = self.config.get("baseUrl")
            .and_then(Json::as_str)
            .unwrap_or("https://opencode.ai/inference/v1");

        // Free tier moved from /zen/v1 to /inference/v1 (OpenCode v2); /zen/v1 now
        // answers every free request with 403 FreeTierError.
        if is_responses_model(model) {
            Ok(format!("{}/inference/v1/responses", base.trim_end_matches('/')))
        } else if is_messages_model(model) {
            Ok(format!("{}/inference/v1/messages", base.trim_end_matches('/')))
        } else {
            Ok(format!("{}/inference/v1/chat/completions", base.trim_end_matches('/')))
        }
    }

    fn build_headers(
        &self,
        credentials: &Credentials,
        stream: bool,
        url: &str,
        _model: &str,
        _body: &Json,
    ) -> Result<nr_core::http::HeaderList> {
        let mut headers = nr_core::http::HeaderList::new();
        headers.insert("Content-Type".into(), "application/json".into());
        headers.insert("Authorization".into(), "Bearer public".into());
        headers.insert("x-opencode-client".into(), OPENCODE_CLIENT.to_string());

        // Extract downstream UA if present
        let raw_headers = credentials.get("rawHeaders")
            .and_then(|h| h.as_object())
            .cloned()
            .unwrap_or_default();

        let mut lower_headers = HashMap::new();
        for (k, v) in raw_headers {
            lower_headers.insert(k.to_lowercase(), v);
        }

        let downstream_ua = lower_headers.get("user-agent")
            .and_then(Json::as_str)
            .unwrap_or("");

        // Check if it's a valid OpenCode UA
        let is_opencode_ua = downstream_ua.contains("opencode") && downstream_ua.contains('/');
        headers.insert("User-Agent".into(), if is_opencode_ua {
            downstream_ua.to_string()
        } else {
            OPENCODE_UA.to_string()
        });

        // Set session headers
        if let Some(session) = credentials.get(SESSION_FIELD).and_then(Json::as_str) {
            headers.insert(SESSION_HEADER.into(), session.to_string());
            headers.insert("x-opencode-session-id".into(), session.to_string());
            headers.insert("x-session-affinity".into(), session.to_string());
            headers.insert("x-session-id".into(), session.to_string());
        }

        headers.insert("x-opencode-project".into(), "global".to_string());
        if stream {
            headers.insert("Accept".into(), "text/event-stream".into());
        }

        if url.ends_with("/messages") {
            headers.insert("anthropic-version".into(), "2023-06-01".into());
        }

        Ok(headers)
    }
}

#[async_trait]
impl Executor for OpencodeExecutor {
    fn provider(&self) -> &str {
        "opencode"
    }

    fn no_auth(&self) -> bool {
        BaseHooks::no_auth(self)
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        // Prepare credentials with session
        let credentials = self.prepare_request_credentials(
            &req.body,
            req.credentials,
            req.provider_session_id.as_deref(),
            req.client_tool.as_deref(),
        );

        // shortcut: TLS fingerprinting impersonation requires wreq-js / boringssl;
        // upstream falls back to standard HTTP fetch when wreq is unavailable.
        let mut req_creds = credentials;
        base_execute(self, ExecRequest {
            credentials: &mut req_creds,
            ..req
        }).await
    }
}

/// Executor for the canonical provider id (aliases are resolved in nr-executors).
pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(OpencodeExecutor::new())
}
