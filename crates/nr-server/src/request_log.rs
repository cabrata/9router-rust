//! Port of `src/lib/requestLog.js`: per-key request permissions and the
//! per-request audit row.
//!
//! The JS `withRequestLog(kind, handler)` is middleware over a Web
//! `Request`/`Response` pair. The Rust shape is split so both halves stay
//! usable:
//!
//! * [`mask_key`], [`is_model_allowed`], [`is_endpoint_allowed`],
//!   [`compute_tps`], [`path_without_api_prefix`], [`check_key_permissions`] —
//!   the same pure decisions, called from the `/v1` preflight.
//! * [`RequestLogContext`] — the mutable accumulator the JS keeps in
//!   AsyncLocalStorage; the caller fills tokens in as usage rows are saved.
//! * [`RequestLogContext::finish`] — writes the `requestLogs` row with the same
//!   status/duration/TTFT/TPS computation.
//! * [`log_upstream_response`] — wraps an `UpstreamResponse` body so TTFT is
//!   observed and the row is finished on EOF, on a stream error, or when the
//!   client drops the stream (JS `cancel` → 499).

use std::sync::Arc;

use bytes::Bytes;
use futures::Stream;
use nr_core::http::UpstreamResponse;
use nr_db::repos::{insert_request_log, record_usage_for_request_log, ApiKey};
use nr_db::Db;
use parking_lot::Mutex;
use serde_json::{json, Value};

/// `ENDPOINT_KINDS` from src/shared/constants/config.js.
pub const ENDPOINT_KINDS: &[&str] = &[
    "chat",
    "embeddings",
    "images",
    "tts",
    "stt",
    "video",
    "search",
    "fetch",
    "systemone",
];

/// `maskKey(k)`: `k.slice(0, 2)…` for short keys, `first6…last4` otherwise.
pub fn mask_key(key: Option<&str>) -> Option<String> {
    let key = key?;
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 10 {
        return Some(format!("{}…", chars.iter().take(2).collect::<String>()));
    }
    let head: String = chars.iter().take(6).collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    Some(format!("{head}…{tail}"))
}

/// `isModelAllowed(model, allowed)`.
///
/// `"cx/*"` matches any model of provider alias `cx`; `"*"` matches all;
/// otherwise exact. Claude Code's `[1m]` context marker is ignored.
pub fn is_model_allowed(model: Option<&str>, allowed: &[String]) -> bool {
    if allowed.is_empty() {
        return true;
    }
    let Some(model) = model else {
        return false;
    };
    // `model.replace(/\[[^\]]*\]$/, "")`
    let trimmed = match model.rfind('[') {
        Some(idx) if model.ends_with(']') => &model[..idx],
        _ => model,
    };
    allowed.iter().any(|a| {
        a == "*" || a == trimmed || (a.ends_with("/*") && trimmed.starts_with(&a[..a.len() - 1]))
    })
}

/// `isEndpointAllowed(kind, allowed)`.
pub fn is_endpoint_allowed(kind: &str, allowed: &[String]) -> bool {
    allowed.is_empty() || allowed.iter().any(|a| a == kind)
}

/// `computeTps(completionTokens, durationMs, ttftMs, stream)`.
///
/// Generation time excludes the time to first token for streams; the result is
/// rounded to one decimal.
pub fn compute_tps(completion_tokens: i64, duration_ms: i64, ttft_ms: i64, stream: bool) -> f64 {
    let gen_ms = if stream && duration_ms > ttft_ms {
        duration_ms - ttft_ms
    } else {
        duration_ms
    };
    if completion_tokens == 0 || gen_ms <= 0 {
        return 0.0;
    }
    let tps = completion_tokens as f64 / (gen_ms as f64 / 1000.0);
    (tps * 10.0).round() / 10.0
}

/// `url.pathname.replace(/^\/api(?=\/v1)/, "")`.
pub fn path_without_api_prefix(path: &str) -> String {
    match path.strip_prefix("/api/v1") {
        Some(rest) => format!("/v1{rest}"),
        None => path.to_string(),
    }
}

/// The denial message from `withRequestLog` for a key that may not call
/// `model` or `kind`. `Ok(())` means the key is allowed.
///
/// Order matches the JS: inactive → expired → endpoint → model.
pub fn check_key_permissions(
    key: &ApiKey,
    kind: &str,
    model: Option<&str>,
    now_ms: i64,
) -> Result<(), String> {
    if !key.is_active {
        return Err("API key is inactive".to_string());
    }
    if let Some(expired_at) = &key.expired_at {
        if let Some(ts) = nr_core::json::parse_time_ms(&json!(expired_at.as_str())) {
            if ts <= now_ms {
                return Err("API key has expired".to_string());
            }
        }
    }
    if !is_endpoint_allowed(kind, &key.allowed_endpoints) {
        return Err(format!("API key not allowed to use endpoint type: {kind}"));
    }
    if let Some(model) = model {
        if !is_model_allowed(Some(model), &key.allowed_models) {
            return Err(format!("API key not allowed to use model: {model}"));
        }
    }
    Ok(())
}

/// The per-request audit accumulator (JS `ctx` inside `withRequestLog`).
#[derive(Clone, Debug)]
pub struct RequestLogContext {
    pub timestamp: String,
    pub api_key_id: Option<String>,
    pub api_key_name: Option<String>,
    pub api_key_masked: Option<String>,
    pub ip: String,
    pub method: String,
    pub path: String,
    pub endpoint_kind: String,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub resolved_model: Option<String>,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub stream: bool,
    pub user_agent: Option<String>,
    pub error: Option<String>,
    /// Wall-clock start (ms) used for duration/TTFT.
    started_ms: i64,
    /// First-byte timestamp (ms), set by the stream wrapper.
    ttft_at_ms: Option<i64>,
}

impl RequestLogContext {
    /// Build a context for one request, mirroring the JS ctx literal.
    pub fn new(endpoint_kind: &str, method: &str, path: &str) -> Self {
        Self {
            timestamp: nr_core::json::now_iso(),
            api_key_id: None,
            api_key_name: None,
            api_key_masked: None,
            ip: "unknown".to_string(),
            method: method.to_string(),
            path: path_without_api_prefix(path),
            endpoint_kind: endpoint_kind.to_string(),
            model: None,
            provider: None,
            resolved_model: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            stream: false,
            user_agent: None,
            error: None,
            started_ms: nr_core::json::now_ms(),
            ttft_at_ms: None,
        }
    }

    /// Fill the identity fields from the presented key + request headers.
    pub fn with_request(mut self, raw_key: Option<&str>, ip: &str, user_agent: Option<&str>) -> Self {
        self.api_key_masked = mask_key(raw_key);
        self.ip = ip.to_string();
        self.user_agent = user_agent.map(|ua| ua.chars().take(200).collect());
        self
    }

    /// Attach the resolved key record (id + name).
    pub fn with_key_record(mut self, id: Option<&str>, name: Option<&str>) -> Self {
        self.api_key_id = id.map(str::to_string);
        self.api_key_name = name.map(str::to_string);
        self
    }

    /// Record the first-byte time (JS `if (!ctx.ttftAt) ctx.ttftAt = Date.now()`).
    pub fn mark_ttft(&mut self) {
        if self.ttft_at_ms.is_none() {
            self.ttft_at_ms = Some(nr_core::json::now_ms());
        }
    }

    /// `recordUsageForRequestLog(entry)`: add tokens from one usage row,
    /// deduping on the same signature as the JS.
    pub fn record_usage(&mut self, entry: &Value) -> bool {
        let mut ctx = json!({
            "promptTokens": self.prompt_tokens,
            "completionTokens": self.completion_tokens,
            "provider": self.provider,
            "resolvedModel": self.resolved_model,
            "_seen": [],
        });
        let recorded = record_usage_for_request_log(Some(&mut ctx), entry);
        if recorded {
            self.prompt_tokens = ctx.get("promptTokens").and_then(Value::as_i64).unwrap_or(0);
            self.completion_tokens = ctx.get("completionTokens").and_then(Value::as_i64).unwrap_or(0);
            self.provider = ctx.get("provider").and_then(Value::as_str).map(str::to_string);
            self.resolved_model = ctx.get("resolvedModel").and_then(Value::as_str).map(str::to_string);
        }
        recorded
    }

    /// Write the `requestLogs` row (JS `finish(status)`), computing duration,
    /// TTFT and TPS the same way.
    pub fn finish(&self, db: &Db, status: u16) {
        let end = nr_core::json::now_ms();
        let duration_ms = (end - self.started_ms).max(0);
        let ttft_ms = self
            .ttft_at_ms
            .map(|t| (t - self.started_ms).max(0))
            .unwrap_or(duration_ms);
        let entry = json!({
            "timestamp": self.timestamp,
            "apiKeyId": self.api_key_id,
            "apiKeyName": self.api_key_name,
            "apiKeyMasked": self.api_key_masked,
            "ip": self.ip,
            "method": self.method,
            "path": self.path,
            "endpointKind": self.endpoint_kind,
            "model": self.model,
            "provider": self.provider,
            "resolvedModel": self.resolved_model,
            "status": status,
            "stream": self.stream,
            "promptTokens": self.prompt_tokens,
            "completionTokens": self.completion_tokens,
            "durationMs": duration_ms,
            "ttftMs": ttft_ms,
            "tps": compute_tps(self.completion_tokens, duration_ms, ttft_ms, self.stream),
            "userAgent": self.user_agent,
            "error": self.error,
        });
        // Fail-open, like the JS: a logging failure must never break the request.
        if let Err(e) = insert_request_log(db, &entry) {
            tracing::warn!("[requestLogs] insert failed: {e}");
        }
    }
}

/// Wrap an upstream response so the body is observed for the request log.
///
/// Status and headers pass through untouched; the wrapped body marks TTFT on
/// the first chunk and finishes the row on EOF, on a stream error, or on drop.
pub fn log_upstream_response(
    db: Db,
    ctx: RequestLogContext,
    resp: UpstreamResponse,
) -> UpstreamResponse {
    let UpstreamResponse { status, headers, body } = resp;

    // A non-2xx body is not streamed to a dashboard reader; finish now.
    if status >= 400 {
        ctx.finish(&db, status);
        return UpstreamResponse { status, headers, body };
    }

    let is_sse = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false);

    let mut ctx = ctx;
    ctx.stream = is_sse;
    let shared = Arc::new(Mutex::new(ctx));

    UpstreamResponse {
        status,
        headers,
        body: Box::pin(LoggedStream {
            inner: body,
            db: Some(db),
            ctx: shared,
            status,
            finished: false,
        }),
    }
}

/// `Stream` wrapper that finishes the request-log row exactly once.
struct LoggedStream {
    inner: nr_core::http::BodyStream,
    db: Option<Db>,
    ctx: Arc<Mutex<RequestLogContext>>,
    status: u16,
    finished: bool,
}

impl LoggedStream {
    fn finish(&mut self, status: u16) {
        if self.finished {
            return;
        }
        self.finished = true;
        if let Some(db) = self.db.take() {
            self.ctx.lock().finish(&db, status);
        }
    }
}

impl Drop for LoggedStream {
    fn drop(&mut self) {
        if !self.finished {
            // Client went away mid-stream: JS reports 499 for a 200 upstream.
            let status = if self.status == 200 { 499 } else { self.status };
            {
                let mut ctx = self.ctx.lock();
                if ctx.error.is_none() {
                    ctx.error = Some("client disconnected".to_string());
                }
            }
            self.finish(status);
        }
    }
}

impl Stream for LoggedStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        let this = &mut *self;
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                this.ctx.lock().mark_ttft();
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(e))) => {
                {
                    let mut ctx = this.ctx.lock();
                    if ctx.error.is_none() {
                        ctx.error = Some(e.to_string());
                    }
                }
                let status = if this.status >= 400 { this.status } else { 500 };
                this.finish(status);
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                let status = this.status;
                this.finish(status);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use nr_db::repos::get_request_logs;
    use nr_db::repos::RequestLogFilter;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn mask_key_rules() {
        assert_eq!(mask_key(None), None);
        assert_eq!(mask_key(Some("sk-12345")), Some("sk…".to_string()));
        // `length <= 10` is the short form; 12 chars keeps head/tail.
        assert_eq!(mask_key(Some("sk-1234567890")), Some("sk-123…7890".to_string()));
        assert_eq!(mask_key(Some("sk-abcdef123456")), Some("sk-abc…3456".to_string()));
    }

    #[test]
    fn model_allow_list_rules() {
        assert!(is_model_allowed(Some("cc/opus"), &[]));
        assert!(is_model_allowed(Some("cc/opus"), &["cc/*".into()]));
        assert!(is_model_allowed(Some("cc/opus[1m]"), &["cc/opus".into()]));
        assert!(!is_model_allowed(Some("gh/gpt"), &["cc/*".into()]));
        assert!(!is_model_allowed(None, &["cc/*".into()]));
        assert!(is_model_allowed(Some("anything"), &["*".into()]));
    }

    #[test]
    fn endpoint_allow_list_rules() {
        assert!(is_endpoint_allowed("chat", &[]));
        assert!(!is_endpoint_allowed("chat", &["embeddings".into()]));
        assert!(is_endpoint_allowed("chat", &["chat".into()]));
    }

    #[test]
    fn tps_matches_js() {
        // 100 tokens over (3000-1000) ms = 50 tok/s
        assert_eq!(compute_tps(100, 3000, 1000, true), 50.0);
        // Non-streams use the full duration.
        assert_eq!(compute_tps(100, 3000, 1000, false), 33.3);
        assert_eq!(compute_tps(0, 3000, 1000, true), 0.0);
        assert_eq!(compute_tps(100, 0, 0, true), 0.0);
        // Stream shorter than TTFT falls back to the full duration.
        assert_eq!(compute_tps(10, 1000, 2000, true), 10.0);
    }

    #[test]
    fn path_prefix_is_stripped_like_js() {
        assert_eq!(
            path_without_api_prefix("/api/v1/chat/completions"),
            "/v1/chat/completions"
        );
        assert_eq!(
            path_without_api_prefix("/v1/chat/completions"),
            "/v1/chat/completions"
        );
        // Only `/api` immediately followed by `/v1` is rewritten.
        assert_eq!(path_without_api_prefix("/api/models"), "/api/models");
    }

    #[test]
    fn finish_writes_a_row() {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();

        let mut ctx = RequestLogContext::new("chat", "POST", "/api/v1/chat/completions")
            .with_request(Some("sk-abcdef123456"), "10.1.2.3", Some("ua/1"))
            .with_key_record(Some("key-1"), Some("Team A"));
        ctx.model = Some("cc/opus".into());
        ctx.stream = true;
        ctx.completion_tokens = 100;
        ctx.mark_ttft();
        ctx.finish(&db, 200);

        let logs = get_request_logs(&db, &RequestLogFilter::default()).unwrap();
        let row = &logs["logs"][0];
        assert_eq!(row["status"], json!(200));
        assert_eq!(row["apiKeyId"], json!("key-1"));
        assert_eq!(row["apiKeyName"], json!("Team A"));
        assert_eq!(row["ip"], json!("10.1.2.3"));
        assert_eq!(row["path"], json!("/v1/chat/completions"));
        assert_eq!(row["model"], json!("cc/opus"));
        assert_eq!(row["endpointKind"], json!("chat"));
        assert_eq!(row["stream"], json!(true));
        // TTFT is marked right before finishing, so generation time is ~0 ms and
        // the JS formula yields 0 rather than infinity.
        assert_eq!(row["tps"].as_f64().unwrap(), 0.0);
    }

    #[test]
    fn record_usage_sums() {
        let mut ctx = RequestLogContext::new("chat", "POST", "/v1/chat/completions");
        let entry = json!({
            "timestamp": "2026-01-01T00:00:00.000Z",
            "provider": "claude",
            "model": "opus",
            "tokens": { "prompt_tokens": 12, "completion_tokens": 34 },
        });
        assert!(ctx.record_usage(&entry));
        assert_eq!(ctx.prompt_tokens, 12);
        assert_eq!(ctx.completion_tokens, 34);
        assert_eq!(ctx.provider.as_deref(), Some("claude"));
        assert_eq!(ctx.resolved_model.as_deref(), Some("opus"));
    }

    #[tokio::test]
    async fn streamed_body_logs_on_eof() {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();

        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(b"data: a\n\n")),
            Ok(Bytes::from_static(b"data: b\n\n")),
        ];
        let body: nr_core::http::BodyStream = Box::pin(futures::stream::iter(chunks));
        let resp = UpstreamResponse::sse(body);

        let ctx = RequestLogContext::new("chat", "POST", "/v1/chat/completions");
        let mut logged = log_upstream_response(db.clone(), ctx, resp);
        let mut seen = 0;
        while let Some(chunk) = logged.body.next().await {
            chunk.unwrap();
            seen += 1;
        }
        assert_eq!(seen, 2);

        let logs = get_request_logs(&db, &RequestLogFilter::default()).unwrap();
        assert_eq!(logs["logs"].as_array().unwrap().len(), 1);
        assert_eq!(logs["logs"][0]["status"], json!(200));
        assert_eq!(logs["logs"][0]["stream"], json!(true));
    }

    #[tokio::test]
    async fn dropped_stream_logs_499() {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();

        let body: nr_core::http::BodyStream = Box::pin(futures::stream::pending());
        let resp = UpstreamResponse::sse(body);
        let ctx = RequestLogContext::new("chat", "POST", "/v1/chat/completions");
        let logged = log_upstream_response(db.clone(), ctx, resp);
        drop(logged);

        let logs = get_request_logs(&db, &RequestLogFilter::default()).unwrap();
        assert_eq!(logs["logs"][0]["status"], json!(499));
        assert_eq!(logs["logs"][0]["error"], json!("client disconnected"));
    }
}
