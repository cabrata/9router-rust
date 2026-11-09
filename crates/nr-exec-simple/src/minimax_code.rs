//! Port of open-sse/executors/minimax-code.js.
//!
//! MiniMax Code reports credit/quota exhaustion as 402/403 (and 429) with a body
//! that names the balance. The executor normalizes 402/403 quota exhaustion to 429
//! for the account-switching logic.
//!
//! Lease connections (authType "mcode-lease") borrow an access token from the
//! MiniMax desktop app's local auth broker instead of holding their own refresh
//! token (MiniMax refresh tokens are single-use). The broker protocol is the
//! length-prefixed JSON frame protocol from open-sse/services/minimaxCodeLease.js.

use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use nr_core::{Credentials, Error, ExecRequest, ExecResult, Executor, Json, Result};
use nr_exec_base::default_executor::DefaultExecutor;

const AUTH_LEASE_PROTOCOL_VERSION: u32 = 1;
const AUTH_LEASE_MAX_FRAME_BYTES: usize = 64 * 1024;
const AUTH_LEASE_MAX_MIN_VALIDITY_MS: u64 = 5 * 60 * 1000;
const REFRESH_MARGIN_MS: i64 = 60 * 1000;
const LEASE_TIMEOUT_MS: u64 = 10_000;

const CAPABILITY_FILE: &str = "mcode-auth-lease-v1.cap";
const SOCKET_FILE: &str = "mcode-auth-lease-v1.sock";

/// Per-data-dir cache of leased tokens, keyed by the resolved data dir.
static LEASE_CACHE: LazyLock<Mutex<std::collections::HashMap<String, LeasedToken>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

struct LeasedToken {
    access_token: String,
    expires_at_ms: i64,
    generation: u64,
}

/// A connection uses lease auth when it was created from the desktop app
/// (authType "mcode-lease") instead of the router's own OAuth device flow.
fn is_lease_connection(credentials: &Credentials) -> bool {
    credentials.get("authType").and_then(Json::as_str) == Some("mcode-lease")
}

/// Resolve the desktop app's data dir: per-connection override → env → default.
fn resolve_mcode_data_dir(credentials: &Credentials) -> PathBuf {
    if let Some(over) = credentials.psd_str("mcodeDataDir") {
        if !over.trim().is_empty() {
            return PathBuf::from(over.trim());
        }
    }
    if let Ok(env) = std::env::var("MINIMAX_DESKTOP_DATA_DIR") {
        if !env.trim().is_empty() {
            return PathBuf::from(env.trim());
        }
    }
    nr_core::paths::home_dir().join(".minimax")
}

/// Read and validate the broker-published capability token.
fn read_capability(data_dir: &std::path::Path) -> Result<String> {
    let capability_file = data_dir.join("run").join(CAPABILITY_FILE);
    let cap = std::fs::read_to_string(&capability_file).map_err(|e| {
        Error::Provider(format!(
            "Cannot read capability file {}: Is the MiniMax Code desktop app installed and logged in on this machine? ({e})",
            capability_file.display()
        ))
    })?;
    let cap = cap.trim().to_string();
    if cap.len() != 43 || !cap.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err(Error::Provider("MiniMax Code capability file has unexpected content.".into()));
    }
    Ok(cap)
}

/// Length-prefixed JSON frame.
fn encode_frame(value: &Json) -> Result<Vec<u8>> {
    let payload = serde_json::to_vec(value).map_err(|e| Error::Provider(e.to_string()))?;
    if payload.is_empty() || payload.len() > AUTH_LEASE_MAX_FRAME_BYTES {
        return Err(Error::Provider("Frame payload out of bounds.".into()));
    }
    let len = payload.len() as u32;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Decode one length-prefixed JSON frame from a buffer. Returns the frame and the
/// number of bytes consumed, or `None` when the buffer does not yet hold a full frame.
fn decode_frame(buffer: &[u8]) -> Result<Option<(Json, usize)>> {
    if buffer.len() < 4 {
        return Ok(None);
    }
    let payload_length = u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;
    if payload_length == 0 || payload_length > AUTH_LEASE_MAX_FRAME_BYTES {
        return Err(Error::Provider("MiniMax Code broker sent an invalid frame length.".into()));
    }
    let frame_length = 4 + payload_length;
    if buffer.len() < frame_length {
        return Ok(None);
    }
    let value: Json = serde_json::from_slice(&buffer[4..frame_length])
        .map_err(|_| Error::Provider("MiniMax Code broker sent a non-JSON frame.".into()))?;
    Ok(Some((value, frame_length)))
}

#[cfg(unix)]
async fn send_lease_request(endpoint: &str, request: &Json) -> Result<Json> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    let mut stream = UnixStream::connect(endpoint).await.map_err(|e| {
        Error::Provider(format!(
            "Cannot reach MiniMax Code auth broker at {endpoint}: {e}. Is the MiniMax Code desktop app running?"
        ))
    })?;

    stream
        .write_all(&encode_frame(request)?)
        .await
        .map_err(|e| Error::Provider(format!("MiniMax Code broker write failed: {e}")))?;

    let deadline = Duration::from_millis(LEASE_TIMEOUT_MS);
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let started = std::time::Instant::now();
    loop {
        if started.elapsed() > deadline {
            return Err(Error::Provider(format!(
                "MiniMax Code broker did not respond within {LEASE_TIMEOUT_MS}ms."
            )));
        }
        let n = tokio::time::timeout(deadline - started.elapsed(), stream.read(&mut chunk))
            .await
            .map_err(|_| Error::Provider(format!("MiniMax Code broker did not respond within {LEASE_TIMEOUT_MS}ms.")))?
            .map_err(|e| Error::Provider(format!("MiniMax Code broker read failed: {e}")))?;
        if n == 0 {
            return Err(Error::Provider("MiniMax Code broker closed the connection early.".into()));
        }
        buffer.extend_from_slice(&chunk[..n]);
        if let Some((value, _)) = decode_frame(&buffer)? {
            return Ok(value);
        }
    }
}

#[cfg(not(unix))]
async fn send_lease_request(_endpoint: &str, _request: &Json) -> Result<Json> {
    // shortcut: Windows named-pipe transport not implemented in this port.
    // Lease mode requires the desktop broker socket/pipe; implement when a
    // Windows-targeted build needs it. Cloud-API connections are unaffected.
    Err(Error::Provider(
        "MiniMax Code desktop lease auth is not available on this platform.".into(),
    ))
}

/// Assert the broker response carried `ok: true`; return the `result` payload.
fn assert_ok(response: &Json) -> Result<&Json> {
    if response.get("ok").and_then(Json::as_bool) == Some(true) {
        return Ok(response.get("result").unwrap_or(&Json::Null));
    }
    let code = response
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(Json::as_str)
        .unwrap_or("INTERNAL_ERROR");
    Err(Error::Provider(format!("MiniMax Code broker auth error: {code}")))
}

/// `requestStatus(dataDir)`: read-only desktop broker probe — is the app
/// installed, running and signed in? Matches `open-sse/services/minimaxCodeLease.js`.
pub async fn request_status(data_dir: &std::path::Path) -> Result<Json> {
    let capability = read_capability(data_dir)?;
    let request_id = format!("9r-status-{}", now_ms().into_base36());
    let request = serde_json::json!({
        "version": AUTH_LEASE_PROTOCOL_VERSION,
        "requestId": request_id,
        "capability": capability,
        "method": "status",
    });
    let socket = data_dir.join("run").join(SOCKET_FILE);
    let response = send_lease_request(&socket.to_string_lossy(), &request).await?;
    let result = assert_ok(&response)?;
    Ok(serde_json::json!({
        "status": result.get("status").cloned().unwrap_or(Json::Null),
        "generation": result.get("generation").cloned().unwrap_or(Json::Null),
        "expiresAtMs": result.get("expiresAtMs").cloned().unwrap_or(Json::Null),
    }))
}

/// Borrow a cached-or-fresh access token from the desktop broker.
async fn get_lease_token(credentials: &Credentials) -> Result<LeasedToken> {
    let data_dir = resolve_mcode_data_dir(credentials);
    let key = data_dir.to_string_lossy().to_string();
    let now = now_ms();

    if let Ok(cache) = LEASE_CACHE.lock() {
        if let Some(tok) = cache.get(&key) {
            if tok.expires_at_ms - now > REFRESH_MARGIN_MS {
                return Ok(LeasedToken {
                    access_token: tok.access_token.clone(),
                    expires_at_ms: tok.expires_at_ms,
                    generation: tok.generation,
                });
            }
        }
    }

    let capability = read_capability(&data_dir)?;
    let request_id = format!("9r-lease-{}", now.into_base36());
    let request = serde_json::json!({
        "version": AUTH_LEASE_PROTOCOL_VERSION,
        "requestId": request_id,
        "capability": capability,
        "method": "lease",
        "minValidityMs": AUTH_LEASE_MAX_MIN_VALIDITY_MS,
    });

    let socket = data_dir.join("run").join(SOCKET_FILE);
    let response = send_lease_request(&socket.to_string_lossy(), &request).await?;
    let result = assert_ok(&response)?;
    let token = LeasedToken {
        access_token: result.get("accessToken").and_then(Json::as_str).unwrap_or("").to_string(),
        expires_at_ms: result.get("expiresAtMs").and_then(Json::as_i64).unwrap_or(0),
        generation: result.get("generation").and_then(Json::as_u64).unwrap_or(0),
    };

    if let Ok(mut cache) = LEASE_CACHE.lock() {
        cache.insert(key, LeasedToken {
            access_token: token.access_token.clone(),
            expires_at_ms: token.expires_at_ms,
            generation: token.generation,
        });
    }
    Ok(token)
}

/// Drop the cached lease so the next call re-borrows (e.g. after an upstream 401).
fn invalidate_lease_token(credentials: &Credentials) {
    let data_dir = resolve_mcode_data_dir(credentials);
    let key = data_dir.to_string_lossy().to_string();
    if let Ok(mut cache) = LEASE_CACHE.lock() {
        cache.remove(&key);
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

trait Base36 {
    fn into_base36(self) -> String;
}
impl Base36 for i64 {
    fn into_base36(self) -> String {
        const CHARS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
        let mut n = self;
        if n == 0 {
            return "0".to_string();
        }
        let mut out = Vec::new();
        while n > 0 {
            out.push(CHARS[(n % 36) as usize]);
            n /= 36;
        }
        out.reverse();
        String::from_utf8(out).unwrap_or_else(|_| "0".to_string())
    }
}

/// Check if text contains quota-related keywords in CN or EN (magpie's word list).
fn has_quota_keywords(text: &str) -> bool {
    let text_lower = text.to_lowercase();
    text_lower.contains("insufficient")
        || text_lower.contains("balance")
        || text_lower.contains("credit")
        || text_lower.contains("quota")
        || text_lower.contains("exhaust")
        || text_lower.contains("limit")
        || text.contains("余额")
        || text.contains("积分")
        || text.contains("额度")
        || text.contains("不足")
        || text.contains("用完")
        || text.contains("上限")
}

/// Normalize quota responses: 402/403 with balance message → 429.
///
/// chatCore treats 401/403 as refresh-and-retry and 429 as switch-to-next-account,
/// so a credits refusal left at 402/403 would keep retrying a half-spent account.
/// A 403 that does NOT name the balance passes through untouched — real auth refusal.
async fn normalize_quota_response(response: nr_core::http::UpstreamResponse) -> nr_core::http::UpstreamResponse {
    let status = response.status;
    if status != 402 && status != 403 && status != 429 {
        return response;
    }

    let mut headers = response.headers.clone();
    headers.remove(http::header::CONTENT_LENGTH);
    headers.remove(http::header::CONTENT_ENCODING);

    // A 429 is already the right signal for the account loop — pass it through as-is.
    if status == 429 {
        let body = response.bytes().await.unwrap_or_default();
        return nr_core::http::UpstreamResponse::from_bytes(status, headers, body);
    }

    let body = response.bytes().await.unwrap_or_default();
    let body_str = String::from_utf8_lossy(&body);

    // 402/403 that don't name the balance are auth failures — likewise untouched.
    if !has_quota_keywords(&body_str) {
        return nr_core::http::UpstreamResponse::from_bytes(status, headers, body);
    }

    let mut msg = body_str.trim().to_string();
    if let Ok(v) = serde_json::from_slice::<Json>(&body) {
        msg = v.get("error")
            .and_then(|e| e.get("message"))
            .and_then(Json::as_str)
            .or_else(|| v.get("message").and_then(Json::as_str))
            .or_else(|| v.get("msg").and_then(Json::as_str))
            .or_else(|| v.get("base_resp").and_then(|br| br.get("status_msg")).and_then(Json::as_str))
            .map(String::from)
            .unwrap_or(msg);
    }

    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );

    let error_body = serde_json::json!({
        "type": "error",
        "error": {
            "type": "rate_limit_error",
            "message": format!("usage limit reached: {msg}")
        }
    });

    let error_bytes = serde_json::to_vec(&error_body).unwrap_or_default();
    nr_core::http::UpstreamResponse::from_bytes(429, headers, error_bytes)
}

/// MiniMax Code executor: extends DefaultExecutor with quota normalization and lease support.
pub struct MinimaxCodeExecutor {
    default: DefaultExecutor,
}

impl MinimaxCodeExecutor {
    fn new(provider: &str) -> Self {
        Self { default: DefaultExecutor::new(provider) }
    }
}

#[async_trait]
impl Executor for MinimaxCodeExecutor {
    fn provider(&self) -> &str {
        self.default.provider()
    }

    fn no_auth(&self) -> bool {
        self.default.no_auth()
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        // Lease connections carry no stored token — borrow one from the desktop broker.
        if is_lease_connection(req.credentials) {
            let lease = get_lease_token(req.credentials).await?;
            req.credentials.set("accessToken", Json::String(lease.access_token));
        }

        let result = self.default.execute(req).await?;
        let normalized_response = normalize_quota_response(result.response).await;
        Ok(ExecResult {
            response: normalized_response,
            ..result
        })
    }
}

/// Executor for the canonical provider id (aliases are resolved in nr-executors).
pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(MinimaxCodeExecutor::new("minimax-code"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lease_connection_detection() {
        assert!(is_lease_connection(&Credentials::from_value(json!({"authType": "mcode-lease"}))));
        assert!(!is_lease_connection(&Credentials::from_value(json!({"authType": "oauth"}))));
        assert!(!is_lease_connection(&Credentials::from_value(json!({}))));
    }

    #[test]
    fn frame_round_trip() {
        let frame = encode_frame(&json!({"method": "lease", "minValidityMs": 5})).unwrap();
        assert_eq!(frame[0], 0);
        assert_eq!(&frame[..4], &(frame.len() as u32 - 4).to_be_bytes());
        let (decoded, consumed) = decode_frame(&frame).unwrap().unwrap();
        assert_eq!(decoded["method"], "lease");
        assert_eq!(consumed, frame.len());
    }

    #[test]
    fn frame_short_buffer_waits() {
        let frame = encode_frame(&json!({"a": 1})).unwrap();
        assert!(decode_frame(&frame[..3]).unwrap().is_none());
    }

    #[test]
    fn base36_formatting() {
        assert_eq!(0i64.into_base36(), "0");
        assert_eq!(35i64.into_base36(), "z");
        assert_eq!(36i64.into_base36(), "10");
    }

    #[test]
    fn assert_ok_rejects_error() {
        let good = json!({"ok": true, "result": {"accessToken": "t", "expiresAtMs": 1, "generation": 1}});
        assert_eq!(assert_ok(&good).unwrap()["accessToken"], "t");
        let bad = json!({"ok": false, "error": {"code": "BROKER_UNAVAILABLE"}});
        assert!(assert_ok(&bad).is_err());
    }
}
