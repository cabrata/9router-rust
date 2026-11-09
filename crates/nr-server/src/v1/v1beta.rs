//! `/v1beta` Gemini-compatible surface.
//!
//! Port of `src/app/api/v1beta/**`:
//! - `GET /v1beta/models` – Gemini-style model list
//! - `POST /v1beta/models/{model}:generateContent` – non-streaming generate
//! - `POST /v1beta/models/{model}:streamGenerateContent` – SSE streaming

use axum::body::Body;
use axum::extract::Path;
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
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
        .route("/v1beta/models", get(get_models).options(options))
        .route("/v1beta/models/{model}", post(post_generate).options(options))
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

async fn check_auth(db: &Db, headers: &HeaderMap, uri: &axum::http::Uri) -> Option<Response> {
    let settings = get_settings(db).unwrap_or_else(|_| json!({}));
    let require_api_key = settings.get("requireApiKey").and_then(Value::as_bool).unwrap_or(true);
    let key = support::extract_key(headers, support::query_api_key(uri).as_deref());

    if !require_api_key {
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
    None
}



/// GET /v1beta/models - Gemini API list. Static per-alias listing with
/// `models/{provider}/{id}` and a duplicate bare `models/{id}` entry for the
/// `gemini` alias.
async fn get_models() -> Response {
    let mut models: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    let add_model = |models: &mut Vec<Value>, seen: &mut std::collections::HashSet<String>, name: &str, display: &str, description: &str, methods: &[&str]| {
        if !seen.insert(name.to_string()) {
            return;
        }
        models.push(json!({
            "name": name,
            "displayName": display,
            "description": description,
            "supportedGenerationMethods": methods,
            "inputTokenLimit": 128000,
            "outputTokenLimit": 8192,
        }));
    };

    for (provider, models_list) in nr_data::provider_models().iter() {
        let Some(list) = models_list.as_array() else { continue };
        for m in list {
            let Some(id) = m.get("id").and_then(Value::as_str) else { continue };
            let name = m.get("name").and_then(Value::as_str).unwrap_or(id);
            add_model(
                &mut models,
                &mut seen,
                &format!("models/{provider}/{id}"),
                name,
                &format!("{provider} model: {name}"),
                &["generateContent"],
            );
            if provider == "gemini" {
                add_model(
                    &mut models,
                    &mut seen,
                    &format!("models/{id}"),
                    name,
                    &format!("Gemini model: {name}"),
                    &["generateContent", "streamGenerateContent"],
                );
            }
        }
    }

    Json(json!({ "models": models })).into_response()
}

/// POST /v1beta/models/{model}[:action]
async fn post_generate(
    axum::Extension(db): axum::Extension<Db>,
    Path(model): Path<String>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(err) = check_auth(&db, &headers, &uri).await {
        return err;
    }
    let Json(body) = match body {
        Some(b) => b,
        None => return support::error_response(400, "Invalid JSON body"),
    };

    // Gemini path is `provider/model:action` or `model:action`. Streaming is
    // decided by the URL action, not a body field.
    let (model_id, stream) = match model.split_once(':') {
        Some((m, action)) if action.contains("streamGenerateContent") => (m.to_string(), true),
        Some((m, _)) => (m.to_string(), false),
        None => (model.clone(), false),
    };

    let chat_body = gemini_to_chat(body, &model_id, stream);
    let req = ChatRequest {
        model: model_id,
        body: chat_body,
        stream,
        headers: header_list(&headers),
        endpoint: "/v1beta/models".into(),
        api_key: support::extract_key(&headers, support::query_api_key(&uri).as_deref()),
    };

    match handle_chat(req).await {
        Ok(resp) => support::upstream_response(resp),
        Err(e) => support::error_response(e.status(), &e.to_string()),
    }
}

/// Translate Gemini `generateContent` body to OpenAI chat shape.
/// The engine translates back to Gemini on the wire; this adapter maps the
/// Gemini SDK's outer wrapper (model is in the URL, not the body).
fn gemini_to_chat(gem: Value, model: &str, stream: bool) -> Value {
    let mut body = json!({ "model": model, "stream": stream });
    if let Some(contents) = gem.get("contents") {
        body["messages"] = contents.clone();
    }
    if let Some(sys) = gem.get("systemInstruction") {
        if let Some(parts) = sys.get("parts") {
            body["system"] = parts.clone();
        }
    }
    if let Some(cfg) = gem.get("generationConfig") {
        body["generation_config"] = cfg.clone();
    }
    if let Some(tools) = gem.get("tools") {
        body["tools"] = tools.clone();
    }
    if let Some(tfc) = gem.get("toolConfig") {
        body["tool_choice"] = tfc.clone();
    }
    body
}
