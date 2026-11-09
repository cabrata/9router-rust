//! Chat endpoints (port of chat.js and messages.js).
//!
//! All POST routes dispatch through [`nr_engine::handle_chat`]; the
//! endpoint path tells the engine which translator format to target.

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use nr_core::http::HeaderList;
use nr_core::json;
use nr_db::repos::{get_api_key_by_key, get_settings};
use nr_db::Db;
use nr_engine::{handle_chat, ChatRequest};
use serde_json::Value;

use crate::support;

pub fn routes() -> Router {
    Router::new()
        .route("/v1/chat/completions", post(post_chat_completions).options(options))
        .route("/v1/messages/count_tokens", post(post_count_tokens).options(options))
        .route("/v1/messages", post(post_messages).options(options))
        .route("/v1/responses/compact", post(post_responses).options(options))
        .route("/v1/responses", post(post_responses).options(options))
        .route("/v1/api/chat", post(post_chat_completions).options(options))
}

async fn options() -> Response {
    support::cors_headers("GET, POST, OPTIONS")
}

fn header_list(headers: &HeaderMap) -> HeaderList {
    let mut out = HeaderList::new();
    for (name, value) in headers {
        if let Ok(v) = value.to_str() {
            out.insert(name.as_str().to_string(), v.to_string());
        }
    }
    out
}

/// Auth + model checks shared by chat routes. Mirrors the `settings.requireApiKey`
/// guard in `handlers/chat.js`: local installs (key off) pass through without a key.
///
/// Returns the resolved key record alongside the response so the caller can
/// attribute the request log (JS `withRequestLog` reads the same record).
async fn preflight(
    db: &Db,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
    body: &Value,
) -> Result<Option<nr_db::repos::ApiKey>, Response> {
    let settings = get_settings(db).unwrap_or_else(|_| json!({}));
    let require_api_key = settings.get("requireApiKey").and_then(Value::as_bool).unwrap_or(true);
    let key = support::extract_key(headers, support::query_api_key(uri).as_deref());
    if !require_api_key {
        // Local mode still honours an identified key's own restrictions.
        if let Some(k) = &key {
            if let Ok(Some(rec)) = get_api_key_by_key(db, k) {
                if !rec.is_active {
                    return Err(support::error_response(401, "API key is inactive"));
                }
                if let Some(err) = key_permissions_error(&rec, body) {
                    return Err(err);
                }
                return Ok(Some(rec));
            }
        }
        if let Some(model) = body.get("model").and_then(Value::as_str) {
            if model.trim().is_empty() {
                return Err(support::error_response(400, "Missing model"));
            }
        }
        return Ok(None);
    }

    let Some(k) = key else {
        return Err(support::error_response(401, "Missing API key"));
    };
    let Ok(Some(rec)) = get_api_key_by_key(db, &k) else {
        return Err(support::error_response(401, "Invalid API key"));
    };
    if !rec.is_active {
        return Err(support::error_response(401, "API key is inactive"));
    }
    if let Some(exp) = &rec.expired_at {
        if let Some(ts) = nr_core::json::parse_time_ms(&json!(exp.as_str())) {
            if ts <= nr_core::json::now_ms() {
                return Err(support::error_response(401, "API key has expired"));
            }
        }
    }
    if let Some(model) = body.get("model").and_then(Value::as_str) {
        if model.trim().is_empty() {
            return Err(support::error_response(400, "Missing model"));
        }
    }
    if let Some(err) = key_permissions_error(&rec, body) {
        return Err(err);
    }
    Ok(Some(rec))
}

/// `withRequestLog`'s 403 gate: endpoint kind + model allow-lists.
fn key_permissions_error(rec: &nr_db::repos::ApiKey, body: &Value) -> Option<Response> {
    let model = body.get("model").and_then(Value::as_str);
    match crate::request_log::check_key_permissions(rec, "chat", model, nr_core::json::now_ms()) {
        Ok(()) => None,
        Err(message) => Some(support::error_response(403, &message)),
    }
}

/// Shared body of `chat/completions`, `messages`, `responses`, `api/chat`.
async fn dispatch_chat(
    db: Db,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Value,
    endpoint: String,
) -> Response {
    let key_record = match preflight(&db, &headers, &uri, &body).await {
        Ok(record) => record,
        Err(resp) => {
            // Denied requests are logged too (JS logs before returning).
            let ctx = request_log_context(&db, &headers, &uri, "chat", &body, None)
                .with_key_record(None, None);
            ctx.finish(&db, resp.status().as_u16());
            return resp;
        }
    };
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .replace("[1m]", "");
    if model.is_empty() {
        return support::error_response(400, "Missing model");
    }
    let stream = body
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let api_key = support::extract_key(&headers, support::query_api_key(&uri).as_deref());
    // Build the log context before `body` moves into the request.
    let ctx = request_log_context(&db, &headers, &uri, "chat", &body, key_record.as_ref());
    let req = ChatRequest {
        model,
        body,
        stream,
        headers: header_list(&headers),
        endpoint,
        api_key,
    };

    match handle_chat(req).await {
        Ok(resp) => support::upstream_response(crate::request_log::log_upstream_response(db.clone(), ctx, resp)),
        Err(e) => {
            let status = e.status();
            ctx.finish(&db, status);
            support::error_response(status, &e.to_string())
        }
    }
}

/// Build the request-log context for one `/v1` call (JS `withRequestLog` ctx).
fn request_log_context(
    _db: &Db,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
    kind: &str,
    body: &Value,
    key_record: Option<&nr_db::repos::ApiKey>,
) -> crate::request_log::RequestLogContext {
    let raw_key = support::extract_key(headers, support::query_api_key(uri).as_deref());
    let user_agent = headers.get("user-agent").and_then(|v| v.to_str().ok());
    // Display-only fallback: without a peer stamp the JS falls back to XFF.
    let ip = crate::routes::auth::get_client_ip(headers, None);
    let mut ctx = crate::request_log::RequestLogContext::new(kind, "POST", uri.path())
        .with_request(raw_key.as_deref(), &ip, user_agent);
    if let Some(rec) = key_record {
        ctx = ctx.with_key_record(Some(&rec.id), rec.name.as_deref());
    }
    ctx.model = body.get("model").and_then(Value::as_str).map(str::to_string);
    ctx
}

async fn post_chat_completions(
    axum::Extension(db): axum::Extension<Db>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Option<Json<Value>>,
) -> Response {
    match body {
        Some(Json(b)) => dispatch_chat(db, headers, uri, b, "/v1/chat/completions".into()).await,
        None => support::error_response(400, "Invalid JSON body"),
    }
}

async fn post_messages(
    axum::Extension(db): axum::Extension<Db>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Option<Json<Value>>,
) -> Response {
    match body {
        Some(Json(b)) => dispatch_chat(db, headers, uri, b, "/v1/messages".into()).await,
        None => support::error_response(400, "Invalid JSON body"),
    }
}

async fn post_responses(
    axum::Extension(db): axum::Extension<Db>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Option<Json<Value>>,
) -> Response {
    match body {
        Some(Json(b)) => dispatch_chat(db, headers, uri, b, "/v1/responses".into()).await,
        None => support::error_response(400, "Invalid JSON body"),
    }
}

/// POST /v1/messages/count_tokens - estimate Anthropic-style input tokens by
/// character count. Port of messages/count_tokens/route.js.
async fn post_count_tokens(
    body: Option<Json<Value>>,
) -> Response {
    let Json(body) = match body {
        Some(b) => b,
        None => return support::error_response(400, "Invalid JSON body"),
    };
    let messages = body.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut total_chars = count_value_chars(body.get("system").unwrap_or(&Value::Null));
    total_chars += count_value_chars(body.get("tools").unwrap_or(&Value::Null));
    for msg in &messages {
        total_chars += count_value_chars(msg);
    }
    let input_tokens = (total_chars + 3) / 4;
    Json(json!({
        "input_tokens": input_tokens,
    })).into_response()
}

fn count_value_chars(v: &Value) -> usize {
    match v {
        Value::Null => 0,
        Value::Bool(b) => b.to_string().len(),
        Value::Number(n) => n.to_string().len(),
        Value::String(s) => s.len(),
        Value::Array(a) => a.iter().map(count_value_chars).sum(),
        Value::Object(o) => o.iter().map(|(k, val)| k.len() + count_value_chars(val)).sum(),
    }
}
