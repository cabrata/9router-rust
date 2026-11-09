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
async fn preflight(
    db: &Db,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
    body: &Value,
) -> Option<Response> {
    let settings = get_settings(db).unwrap_or_else(|_| json!({}));
    let require_api_key = settings.get("requireApiKey").and_then(Value::as_bool).unwrap_or(true);
    let key = support::extract_key(headers, support::query_api_key(uri).as_deref());
    if !require_api_key {
        if let Some(k) = &key {
            if let Ok(Some(rec)) = get_api_key_by_key(db, k) {
                if !rec.is_active {
                    return Some(support::error_response(401, "API key is inactive"));
                }
            }
        }
        if let Some(model) = body.get("model").and_then(Value::as_str) {
            if model.trim().is_empty() {
                return Some(support::error_response(400, "Missing model"));
            }
        }
        return None;
    }

    let Some(k) = key else {
        return Some(support::error_response(401, "Missing API key"));
    };
    let Ok(Some(rec)) = get_api_key_by_key(db, &k) else {
        return Some(support::error_response(401, "Invalid API key"));
    };
    if !rec.is_active {
        return Some(support::error_response(401, "API key is inactive"));
    }
    if let Some(exp) = &rec.expired_at {
        if let Some(ts) = nr_core::json::parse_time_ms(&json!(exp.as_str())) {
            if ts <= nr_core::json::now_ms() {
                return Some(support::error_response(401, "API key has expired"));
            }
        }
    }
    if let Some(model) = body.get("model").and_then(Value::as_str) {
        if model.trim().is_empty() {
            return Some(support::error_response(400, "Missing model"));
        }
        if !rec.allowed_models.is_empty() && !model_allowed(&model.replace("[1m]", ""), &rec.allowed_models) {
            return Some(support::error_response(403, &format!("API key not allowed to use model: {model}")));
        }
    }
    None
}

fn model_allowed(model: &str, allowed: &[String]) -> bool {
    allowed.iter().any(|a| {
        a == "*" || a == model || (a.ends_with("/*") && model.starts_with(&a[..a.len() - 1]))
    })
}

/// Shared body of `chat/completions`, `messages`, `responses`, `api/chat`.
async fn dispatch_chat(
    db: Db,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Value,
    endpoint: String,
) -> Response {
    if let Some(err) = preflight(&db, &headers, &uri, &body).await {
        return err;
    }
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
    let req = ChatRequest {
        model,
        body,
        stream,
        headers: header_list(&headers),
        endpoint,
        api_key,
    };
    match handle_chat(req).await {
        Ok(resp) => support::upstream_response(resp),
        Err(e) => support::error_response(e.status(), &e.to_string()),
    }
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
