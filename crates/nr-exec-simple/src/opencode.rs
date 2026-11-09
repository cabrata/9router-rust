//! Port of open-sse/executors/opencode.js.
//!
//! OpenCode v2 free-tier with session management, responses API support, and TLS fingerprinting.
//! TODO(dedupe): Implement opencodeTlsFetch and TLS fingerprinting from
//! services/opencodeTlsClient.js and utils/opencodeFingerprint.js once available.

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

static EMPTY: LazyLock<Json> = LazyLock::new(|| Json::Object(Default::default()));

// Session cache for stable session IDs per identity
static SESSION_CACHE: Mutex<HashMap<String, String>> = Mutex::new(HashMap::new());

/// Generate a session ID with timestamp-based prefix and random suffix.
fn generate_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    // Session prefix = 12 hex of ~(timeMs * 0x1000 + counter)
    let session_prefix_mask = (1u64 << 48) - 1;
    let descending_time = (~(now_ms * 0x1000 + 1) & session_prefix_mask) as u64;
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

/// Translate a session ID to opencode format.
fn translate_session_id(session_id: &str, _client_tool: &str) -> String {
    // For now, just return a generated session. Full implementation would hash the input.
    generate_session_id()
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
    _provider_session_id: Option<&str>,
    client_tool: Option<&str>,
) -> String {
    // 1. Try native session from headers
    if let Some(native) = native_session(credentials) {
        return native;
    }

    // 2. Try provider session ID
    if let Some(sid) = _provider_session_id {
        if let Some(normalized) = normalize_session(sid) {
            return translate_session_id(&normalized, client_tool.unwrap_or("generic"));
        }
    }

    // 3. Check if body has session hints
    let has_hints = body.get("session_id").and_then(Json::as_str).is_some()
        || body.get("conversation_id").and_then(Json::as_str).is_some()
        || body.get("prompt_cache_key").and_then(Json::as_str).is_some()
        || (body.get("metadata")
            .and_then(|m| m.get("user_id"))
            .and_then(Json::as_str)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false));

    if has_hints {
        // TODO: Use resolveSessionId from utils/sessionManager.js when available
        return generate_session_id();
    }

    // 4. Use stable session for connection
    stable_session_id(credentials)
}

struct OpencodeExecutor {
    config: &'static Json,
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
        creds.insert(SESSION_FIELD.to_string(), Json::String(session));
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

        // TODO: Implement isResponsesModel and isMessagesModel checks
        // For now, default to /chat/completions
        Ok(format!("{}/chat/completions", base.trim_end_matches('/')))
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
            None,
            None,
        );

        // TODO: Implement TLS fingerprinting using opencodeTlsFetch when available
        // For now, use the standard base_execute path

        base_execute(self, ExecRequest {
            credentials: &credentials,
            ..req
        }).await
    }
}

/// Executor for the canonical provider id (aliases are resolved in nr-executors).
pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(OpencodeExecutor::new())
}
