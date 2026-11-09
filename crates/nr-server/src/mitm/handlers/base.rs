//! Shared handler plumbing: forward to the local 9Router and pipe the reply back.
//! Port of src/mitm/handlers/base.js.
//!
//! The 9Router instance doing the translation is the same process, or a
//! sibling one at `MITM_ROUTER_BASE` (the JS build defaults to
//! `http://localhost:20128`).

use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use futures::StreamExt;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};

use nr_core::{Error, Result};

pub const DEFAULT_LOCAL_ROUTER: &str = "http://localhost:20128";

/// Headers that must not be forwarded to 9Router: hop-by-hop, or overridden by us.
const STRIP_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "connection",
    "transfer-encoding",
    "content-type",
    "authorization",
    "accept-encoding",
];

/// Resolve the router base URL: `$MITM_ROUTER_BASE` else `http://localhost:20128`.
pub fn router_base() -> String {
    let raw = std::env::var("MITM_ROUTER_BASE").unwrap_or_default();
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        DEFAULT_LOCAL_ROUTER.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Optional router API key from `$ROUTER_API_KEY`.
pub fn router_api_key() -> Option<String> {
    std::env::var("ROUTER_API_KEY").ok().filter(|k| !k.is_empty())
}

/// Shared HTTP client. Reused so TLS sessions and the connection pool survive
/// across intercepted requests (an IDE sends one per keystroke).
static CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        // Streaming responses (SSE) may legitimately idle a long time mid-stream;
        // only bound the wait for response headers, not the whole body.
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
});

/// Global client for outbound calls to the router and to real upstreams.
pub fn client() -> &'static reqwest::Client {
    &CLIENT
}

/// Client for pass-through calls to the real upstream.
///
/// server.js dials upstreams with `rejectUnauthorized: false` (a MITM box
/// already terminated the client's TLS, and IDEs pin their own trust roots),
/// so verification must stay off here too or every intercepted request fails.
static INSECURE_CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(600))
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
});

/// Pass-through client: same as [`client`] but without certificate verification.
pub fn insecure_client() -> &'static reqwest::Client {
    &INSECURE_CLIENT
}

/// Copy client headers onto an outbound request, minus hop-by-hop/overridden ones.
pub fn forwarded_headers(client_headers: &HeaderMap) -> Vec<(HeaderName, HeaderValue)> {
    client_headers
        .iter()
        .filter(|(name, _)| {
            let n = name.as_str();
            !STRIP_HEADERS.iter().any(|s| s.eq_ignore_ascii_case(n))
                // `x-request-source: local` marks our own traffic; never echo it back.
                && !n.eq_ignore_ascii_case("x-request-source")
        })
        .map(|(n, v)| (n.clone(), v.clone()))
        .collect()
}

/// POST `body` to `router_base` + `path` and return the raw response.
pub async fn fetch_router(
    router_base: &str,
    path: &str,
    api_key: Option<&str>,
    body: &serde_json::Value,
    client_headers: &HeaderMap,
) -> Result<reqwest::Response> {
    let url = format!("{router_base}{path}");
    let mut req = client()
        .post(&url)
        .header("content-type", "application/json")
        .json(body);
    for (name, value) in forwarded_headers(client_headers) {
        req = req.header(name, value);
    }
    if let Some(key) = api_key {
        req = req.bearer_auth(key);
    }
    req.send()
        .await
        .map_err(|e| Error::other(format!("router {path}: {e}")))
}

/// Build a client-facing response that streams `upstream` through unchanged.
///
/// Status and headers are copied verbatim (minus hop-by-hop), so an error status
/// from the router reaches the IDE with its body intact.
pub fn stream_through(upstream: reqwest::Response, dumper: Option<crate::mitm::logger::ResponseDumper>) -> Response {
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let mut response = Response::builder().status(status);
    for (name, value) in headers.iter() {
        let n = name.as_str();
        if matches!(n, "connection" | "transfer-encoding" | "keep-alive" | "content-length") {
            continue;
        }
        response = response.header(name, value);
    }
    // SSE through a buffering proxy would defeat the point; mirror JS's X-Accel-Buffering.
    let is_sse = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.contains("text/event-stream"));
    if is_sse {
        response = response.header("cache-control", "no-cache");
        response = response.header("x-accel-buffering", "no");
    }

    let mut dumper = dumper;
    let stream = upstream.bytes_stream().map(move |chunk| match chunk {
        Ok(bytes) => {
            if let Some(d) = dumper.as_mut() {
                d.write_chunk(&bytes);
            }
            Ok::<Bytes, std::io::Error>(bytes)
        },
        Err(e) => {
            // A mid-stream upstream failure ends the body: the client sees a
            // truncated response rather than a hang, matching node's pipe().
            if let Some(d) = dumper.take() {
                d.finish();
            }
            Err(std::io::Error::other(e))
        }
    });

    response
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| error_response(StatusCode::BAD_GATEWAY, "Bad Gateway"))
}

/// Read an SSE response from the router and rewrite it chunk-by-chunk.
///
/// `transform` receives each parsed `data:` JSON payload (or `None` on flush) and
/// returns the raw frames to write to the client (e.g. EventStream bytes).
/// The response body owns the transform, so each intercepted request gets its own.
pub fn transform_sse<F>(upstream: reqwest::Response, status: StatusCode, content_type: &'static str, transform: F) -> Response
where
    F: FnMut(Option<serde_json::Value>) -> Vec<Vec<u8>> + Send + 'static,
{
    let byte_stream = upstream.bytes_stream();
    let stream = sse_frames(byte_stream, transform);

    Response::builder()
        .status(status)
        .header("content-type", content_type)
        .header("cache-control", "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| error_response(StatusCode::BAD_GATEWAY, "Bad Gateway"))
}

/// Split an SSE byte stream into `data:` frames and push each through `transform`.
/// Shared by `transform_sse` and kiro's stateful variant.
fn sse_frames<F>(byte_stream: impl futures::Stream<Item = reqwest::Result<Bytes>> + Send + 'static, mut transform: F) -> impl futures::Stream<Item = std::result::Result<Bytes, std::io::Error>>
where
    F: FnMut(Option<serde_json::Value>) -> Vec<Vec<u8>> + Send + 'static,
{
    async_stream::stream! {
        let mut buffer = String::new();
        let mut byte_stream = std::pin::pin!(byte_stream);
        while let Some(chunk) = byte_stream.next().await {
            let Ok(chunk) = chunk else { break };
            // Lossy decode is safe here: SSE `data:` lines are JSON text, and a
            // multi-byte char split across chunks is reassembled by the newline
            // framing below before it is parsed.
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(idx) = buffer.find('\n') {
                let line = buffer[..idx].to_string();
                buffer.drain(..=idx);
                let trimmed = line.trim();
                if trimmed.is_empty() || !trimmed.starts_with("data:") {
                    continue;
                }
                let data = trimmed[5..].trim();
                // `[DONE]` is OpenAI's sentinel; the target format emits its own
                // termination frame on flush instead.
                if data == "[DONE]" {
                    continue;
                }
                let parsed = match serde_json::from_str::<serde_json::Value>(data) {
                    Ok(v) => v,
                    // Unparseable lines are skipped, as in JS — a single bad frame
                    // must not kill an in-flight stream.
                    Err(_) => continue,
                };
                for frame in transform(Some(parsed)) {
                    yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
                }
            }
        }
        for frame in transform(None) {
            yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
        }
    }
}

/// JSON error body in the shape IDE SDKs parse: `{ error: { message, type } }`.
pub fn error_response(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({
        "error": { "message": message, "type": "mitm_error" }
    });
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// SSE-shaped error, so a streaming SDK fails cleanly instead of hanging.
pub fn sse_error_response(message: &str) -> Response {
    let payload = serde_json::json!({ "error": { "message": message } });
    let body = format!("data: {payload}\r\n\r\n");
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_hop_by_hop_and_credentials() {
        let mut h = HeaderMap::new();
        h.insert("host", HeaderValue::from_static("api2.cursor.sh"));
        h.insert("authorization", HeaderValue::from_static("Bearer secret"));
        h.insert("content-type", HeaderValue::from_static("application/json"));
        h.insert("x-request-source", HeaderValue::from_static("local"));
        h.insert("x-stainless-lang", HeaderValue::from_static("js"));
        let out = forwarded_headers(&h);
        let names: Vec<&str> = out.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["x-stainless-lang"]);
    }

    #[test]
    fn router_base_defaults_and_trims() {
        std::env::set_var("MITM_ROUTER_BASE", "http://127.0.0.1:9000///");
        assert_eq!(router_base(), "http://127.0.0.1:9000");
        std::env::set_var("MITM_ROUTER_BASE", "   ");
        assert_eq!(router_base(), DEFAULT_LOCAL_ROUTER);
        std::env::remove_var("MITM_ROUTER_BASE");
    }
}
