//! Shared request helpers for the /v1 gateway routes.
//!
//! Port of the surfaces the JS /v1 route files use from `@/lib/requestLog.js`
//! (`extractKey`, `maskKey`, key-record lookups) and `open-sse/utils/error.js`
//! (`buildErrorBody`, `errorResponse`).

use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

/// `extractKey(request)`: Authorization `Bearer`, then `x-api-key`,
/// `x-goog-api-key`, then the `key` query param.
pub fn extract_key(headers: &HeaderMap, query: Option<&str>) -> Option<String> {
    let auth = headers.get("authorization").and_then(|v| v.to_str().ok());
    if let Some(a) = auth {
        if let Some(rest) = a.strip_prefix("Bearer ") {
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }
    for name in ["x-api-key", "x-goog-api-key"] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()).filter(|s| !s.is_empty()) {
            return Some(v.to_string());
        }
    }
    query.map(|q| q.to_string()).filter(|s| !s.is_empty())
}

/// The `key` query param, if present.
pub fn query_api_key(uri: &axum::http::Uri) -> Option<String> {
    let query = uri.query()?;
    let mut found = None;
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == "key" {
                found = Some(v.to_string());
            }
        }
    }
    found.filter(|s| !s.is_empty())
}

/// `buildErrorBody` for the OpenAI-compatible error shape. Uses the same
/// type/code table as the JS (`errorConfig.js`) via nr-data.
pub fn build_error_body(status: u16, message: &str) -> Value {
    let error_types = nr_data::export("open-sse/config/errorConfig", "ERROR_TYPES")
        .and_then(|e| e.get(&status.to_string()))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_else(|| {
            if status >= 500 {
                serde_json::Map::from_iter([("type".into(), json!("server_error")), ("code".into(), json!("internal_server_error"))])
            } else {
                serde_json::Map::from_iter([("type".into(), json!("invalid_request_error")), ("code".into(), json!(""))])
            }
        });

    let default_messages = nr_data::export("open-sse/config/errorConfig", "DEFAULT_ERROR_MESSAGES");
    let default_msg = match default_messages {
        Some(Value::Object(o)) => o.get(&status.to_string()).cloned().unwrap_or_else(|| json!("An error occurred")),
        _ => json!("An error occurred"),
    };

    // message may be empty → fall back to default message
    let msg = if message.is_empty() { default_msg.as_str().unwrap_or("An error occurred").to_string() } else { message.to_string() };

    json!({
        "error": {
            "message": msg,
            "type": error_types.get("type").unwrap_or(&json!("invalid_request_error")),
            "code": error_types.get("code").unwrap_or(&json!("")),
        }
    })
}

/// `errorResponse(status, message, extraHeaders)`: non-streaming JSON error
/// with CORS plus any extra headers the caller needs (e.g. `Retry-After`).
pub fn error_response_with_headers(status: u16, message: &str, extra: &[(&str, String)]) -> Response {
    let mut resp = error_response(status, message);
    for (name, value) in extra {
        if let (Ok(name), Ok(value)) = (
            axum::http::HeaderName::try_from(*name),
            axum::http::HeaderValue::try_from(value.as_str()),
        ) {
            resp.headers_mut().insert(name, value);
        }
    }
    resp
}

/// `errorResponse(status, message)`: non-streaming JSON error with CORS.
pub fn error_response(status: u16, message: &str) -> Response {
    let body = build_error_body(status, message);
    let mut resp = Json(body).into_response();
    *resp.status_mut() = axum::http::StatusCode::from_u16(status).unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    let headers = resp.headers_mut();
    headers.insert(axum::http::header::CONTENT_TYPE, axum::http::HeaderValue::from_static("application/json"));
    headers.insert(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN, axum::http::HeaderValue::from_static("*"));
    resp
}

/// Convert a `nr_core::http::UpstreamResponse` into an axum `Response`, copying
/// status/headers and streaming body directly.
pub fn upstream_response(resp: nr_core::http::UpstreamResponse) -> Response {
    let nr_core::http::UpstreamResponse { status, headers, body } = resp;
    let status = axum::http::StatusCode::from_u16(status).unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    let mut resp = Response::new(axum::body::Body::from_stream(body));
    *resp.status_mut() = status;
    *resp.headers_mut() = headers;
    resp
}

/// CORS preflight headers for a route.
pub fn cors_headers(methods: &str) -> Response {

    let mut resp = Response::new(axum::body::Body::empty());
    *resp.status_mut() = axum::http::StatusCode::NO_CONTENT;
    let headers = resp.headers_mut();
    headers.insert(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN, axum::http::HeaderValue::from_static("*"));
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_METHODS,
        axum::http::HeaderValue::from_str(methods).unwrap_or_else(|_| axum::http::HeaderValue::from_static("GET, POST, OPTIONS")),
    );
    headers.insert(axum::http::header::ACCESS_CONTROL_ALLOW_HEADERS, axum::http::HeaderValue::from_static("*"));
    resp
}
