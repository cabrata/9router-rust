//! Port of open-sse/executors/opencode-zen.js.
//!
//! OpenCode Zen (free tier) with session management, fingerprinting, and free-tier gates.

use std::sync::Arc;

use async_trait::async_trait;
use nr_core::{ExecRequest, ExecResult, Executor, Json, Result, Credentials};
use nr_exec_base::base::{base_execute, BaseHooks};
use std::sync::LazyLock;

const RESPONSES_BASE_URL: &str = "https://opencode.ai/zen/v1/responses";
const SESSION_HEADER: &str = "x-opencode-session";
const SESSION_FIELD: &str = "_opencodeZenSession";
const OPENCODE_UA: &str = "opencode/1.18.31";
const BASE62_CHARS: &str = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

static EMPTY: LazyLock<Json> = LazyLock::new(|| Json::Object(Default::default()));

/// Fingerprint tools required by free-tier gate.
const FINGERPRINT_TOOLS: &[&str] = &["bash", "glob", "grep", "read"];

/// Translate a session ID to opencode-zen format.
fn translate_session_id(session_id: &str, client_tool: &str) -> String {
    use sha2::{Sha256, Digest};

    let key = format!("opencode\0{}\0{}", client_tool, session_id);
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();

    // Deterministic time field from the digest
    let time_bytes = &digest.as_slice()[0..5];
    let mut time_u64 = 0u64;
    for (i, &b) in time_bytes.iter().enumerate() {
        time_u64 |= (b as u64) << (32 - i * 8);
    }

    let session_prefix_mask = (1u64 << 48) - 1;
    let session_time_mask = (1u64 << 36) - 1;
    let time = time_u64 & session_time_mask;

    // Generate session ID with descending time prefix
    let descending_time = (!(time * 0x1000 + 1) & session_prefix_mask) as u64;
    let hex_time = format!("{:012x}", descending_time);

    let mut random_part = String::new();
    for i in 6..20 {
        let idx = (digest[i] as usize) % 62;
        random_part.push(BASE62_CHARS.chars().nth(idx).unwrap());
    }

    format!("ses_{}{}", hex_time, random_part)
}

/// Check if OpenCode version string is valid (>= 1.17).
fn has_valid_opencode_version(ua: &str) -> bool {
    if let Some(caps) = ua.split('/').nth(1) {
        if let Some(major_str) = caps.split('.').next() {
            if let Ok(major) = major_str.parse::<u32>() {
                if major > 1 {
                    return true;
                }
                if major == 1 {
                    if let Some(minor_str) = caps.split('.').nth(1) {
                        if let Ok(minor) = minor_str.parse::<u32>() {
                            return minor >= 17;
                        }
                    }
                }
            }
        }
    }
    false
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

fn generate_request_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    use rand::Rng;

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let current = now_ms * 0x1000 + 1;
    let time = format!("{:012x}", current);

    let mut random_part = String::new();
    let mut rng = rand::thread_rng();
    for _ in 0..14 {
        let idx = rng.gen_range(0..62);
        random_part.push(BASE62_CHARS.chars().nth(idx).unwrap());
    }
    format!("msg_{}{}", time, random_part)
}

struct OpencodeZenExecutor {
    config: &'static Json,
}

impl OpencodeZenExecutor {
    fn new() -> Self {
        let config = nr_data::provider("opencode-zen")
            .or_else(|| nr_data::provider("openai"))
            .unwrap_or(&EMPTY);
        Self { config }
    }

    fn prepare_request_credentials(
        &self,
        _body: &Json,
        credentials: &Credentials,
        _provider_session_id: Option<&str>,
        client_tool: Option<&str>,
    ) -> Credentials {
        let mut creds = credentials.clone();

        // Try to get native session from headers
        if let Some(native) = native_session(&creds) {
            creds.set(SESSION_FIELD, Json::String(native));
            return creds;
        }

        // Resolve session ID using session manager
        let raw_headers = creds.get("rawHeaders").and_then(|h| h.as_object());
        let resolved = nr_common::session_manager::resolve_session_id(
            raw_headers,
            _body,
            creds.connection_id(),
            None,
            "opencode-zen",
        );
        let translated = translate_session_id(&resolved, client_tool.unwrap_or("generic"));
        creds.set(SESSION_FIELD, Json::String(translated));
        creds
    }
}

impl BaseHooks for OpencodeZenExecutor {
    fn provider(&self) -> &str {
        "opencode-zen"
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
        if nr_data::model_helpers::is_muse_spark_model(Some(model)) {
            Ok(RESPONSES_BASE_URL.to_string())
        } else {
            Ok(format!("{}/chat/completions", "https://opencode.ai/zen/v1"))
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
        let mut headers = nr_exec_base::base::default_build_headers(self, credentials, stream);

        // Set User-Agent from downstream or fallback to official
        if let Some(raw_headers) = credentials.get("rawHeaders").and_then(|h| h.as_object()) {
            if let Some(ua) = raw_headers.get("user-agent")
                .or_else(|| raw_headers.get("User-Agent"))
                .and_then(Json::as_str) {
                if has_valid_opencode_version(ua) {
                    headers.insert("User-Agent".into(), ua.to_string());
                } else {
                    headers.insert("User-Agent".into(), OPENCODE_UA.to_string());
                }
            } else {
                headers.insert("User-Agent".into(), OPENCODE_UA.to_string());
            }
        } else {
            headers.insert("User-Agent".into(), OPENCODE_UA.to_string());
        }

        // Set session headers
        if let Some(session) = credentials.get(SESSION_FIELD).and_then(Json::as_str) {
            headers.insert(SESSION_HEADER.into(), session.to_string());
        }

        // Free-tier gate: use Bearer public for free-tier models
        if nr_data::model_helpers::is_opencode_free_tier_model(Some(_model)) {
            headers.insert("Authorization".into(), "Bearer public".into());
            headers.insert("x-opencode-request".into(), generate_request_id());
        }

        if url.contains("/messages") {
            headers.insert("anthropic-version".into(), "2023-06-01".into());
        }

        Ok(headers)
    }
}

#[async_trait]
impl Executor for OpencodeZenExecutor {
    fn provider(&self) -> &str {
        "opencode-zen"
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
    Arc::new(OpencodeZenExecutor::new())
}
