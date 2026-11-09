//! Integration tests for the `/v1` + `/v1beta` gateway surface.
//!
//! Chat routes run against a mock engine installed via
//! [`nr_engine::set_chat_engine`], so no upstream network is touched. The
//! key-auth guard (`settings.requireApiKey`, default `true`) is exercised both
//! ways. Port of `src/app/api/v1/**`, `src/app/api/v1beta/**`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{header, HeaderValue, Request, StatusCode};
use nr_core::http::UpstreamResponse;
use nr_core::{json, Result};
use nr_db::repos::{create_provider_connection, update_settings};
use nr_db::Db;
use nr_engine::{ChatEngine, ChatRequest};
use serde_json::Value;
use tempfile::TempDir;
use tower::ServiceExt;

/// Captures the latest request and replays a canned outcome. `UpstreamResponse`
/// is not `Clone`, so the outcome is produced by a closure per call.
struct MockEngine {
    next: Box<dyn Fn() -> Result<UpstreamResponse> + Send + Sync>,
    last: Mutex<Option<ChatRequest>>,
    calls: AtomicUsize,
}

impl MockEngine {
    fn json(status: u16, body: &Value) -> Self {
        let body = body.clone();
        Self {
            next: Box::new(move || Ok(UpstreamResponse::json(status, &body))),
            last: Mutex::new(None),
            calls: AtomicUsize::new(0),
        }
    }

    fn err(status: u16, message: &str) -> Self {
        let msg = message.to_string();
        Self {
            next: Box::new(move || Err(nr_core::Error::Upstream { status, message: msg.clone() })),
            last: Mutex::new(None),
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl ChatEngine for MockEngine {
    async fn handle_chat(&self, req: ChatRequest) -> Result<UpstreamResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last.lock().unwrap() = Some(req);
        (self.next)()
    }
}

/// The active chat engine is a process-global static, so tests that install a
/// mock must not run concurrently. Every test in this file takes this lock.
static ENGINE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    ENGINE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn install(engine: MockEngine) -> Arc<MockEngine> {
    let e = Arc::new(engine);
    nr_engine::set_chat_engine(e.clone());
    e
}

/// A fresh router over a fresh temp DB with `requireApiKey` off, so the
/// preflight guard passes and the request reaches the engine.
async fn app_with(require_api_key: bool) -> (TempDir, axum::Router, Db) {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    update_settings(&db, json!({ "requireApiKey": require_api_key })).unwrap();
    let router = nr_server::v1::v1_router().layer(axum::Extension(db.clone()));
    (dir, router, db)
}

async fn call(
    router: axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    key: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(k) = key {
        builder = builder.header("authorization", format!("Bearer {k}"));
    }
    let req = match body {
        Some(b) => builder
            .header(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .body(Body::from(b.to_string())),
        None => builder.body(Body::empty()),
    }
    .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 10_000_000).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

fn openai_ok_body() -> Value {
    json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "model": "openai/gpt-4o",
        "choices": [{ "index": 0, "message": { "role": "assistant", "content": "hi there" }, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7 }
    })
}

// ─── /v1/models ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn models_list_requires_key_by_default() {
    let _g = lock();
    let (_d, router, _db) = app_with(true).await;

    let (s, body) = call(router.clone(), "GET", "/v1/models", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"]["message"], "Missing API key");
    // 401 maps to the auth error type per errorConfig.ERROR_TYPES.
    assert_eq!(body["error"]["type"], "authentication_error");
    assert_eq!(body["error"]["code"], "invalid_api_key");

    // An unknown key is rejected too.
    let (s, body) = call(router.clone(), "GET", "/v1/models", None, Some("nope")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"]["message"], "Invalid API key");
}

#[tokio::test]
async fn models_list_openai_shape_with_connection() {
    let _g = lock();
    let (_d, router, db) = app_with(false).await;
    create_provider_connection(
        &db,
        &json!({
            "id": "c1", "provider": "openai", "authType": "apikey",
            "name": "c1", "priority": 1, "isActive": true,
            "providerSpecificData": { "apiKey": "sk-1" }
        }),
    )
    .unwrap();

    let (s, body) = call(router.clone(), "GET", "/v1/models", None, None).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["object"], "list");
    let data = body["data"].as_array().expect("data array");
    assert!(!data.is_empty(), "openai connection should yield static models");
    for m in data {
        assert_eq!(m["object"], "model");
        assert!(m["id"].as_str().unwrap().contains('/'));
        assert_eq!(m["owned_by"], "openai");
    }

    // Single-model lookup resolves through the same catalog.
    let id = data[0]["id"].as_str().unwrap().to_string();
    let (s, body) = call(router.clone(), "GET", &format!("/v1/models/{id}"), None, None).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["id"], json!(id));

    // Unknown model is a 404 with the OpenAI error envelope.
    let (s, body) = call(router.clone(), "GET", "/v1/models/openai/nope-xyz", None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{body}");
    assert!(body["error"]["message"].as_str().unwrap().contains("nope-xyz"));
}

/// BUG(nr-server): a combo created without `kind` is stored as `""` by
/// `nr_db::repos::create_combo` (`kind.clone().unwrap_or_default()`), and
/// `build_catalog` filters with `c.kind.as_deref().unwrap_or("llm")`. Because
/// `Some("")` is not `"llm"`, such combos never appear in `/v1/models`.
/// JS treats a missing/empty kind as `LLM_KIND` (`combo?.kind || LLM_KIND` in
/// src/app/api/v1/models/route.js:271), so they do show up there.
#[tokio::test]
async fn models_list_includes_combo_without_kind() {
    let _g = lock();
    let (_d, router, db) = app_with(false).await;
    nr_db::repos::create_combo(&db, &json!({ "name": "my-combo", "models": ["openai/gpt-4o"] })).unwrap();

    let (s, body) = call(router.clone(), "GET", "/v1/models", None, None).await;
    assert_eq!(s, StatusCode::OK);
    let ids: Vec<&str> = body["data"].as_array().unwrap().iter().filter_map(|m| m["id"].as_str()).collect();
    assert!(ids.contains(&"my-combo"), "combo missing from catalog: {ids:?}");
}

/// The same listing works when the combo declares `kind: "llm"` explicitly,
/// which proves the catalog path itself is sound.
#[tokio::test]
async fn models_list_includes_combo_with_explicit_kind() {
    let _g = lock();
    let (_d, router, db) = app_with(false).await;
    nr_db::repos::create_combo(
        &db,
        &json!({ "name": "typed-combo", "kind": "llm", "models": ["openai/gpt-4o"] }),
    )
    .unwrap();

    let (s, body) = call(router.clone(), "GET", "/v1/models", None, None).await;
    assert_eq!(s, StatusCode::OK);
    let ids: Vec<&str> = body["data"].as_array().unwrap().iter().filter_map(|m| m["id"].as_str()).collect();
    assert!(ids.contains(&"typed-combo"), "{ids:?}");
}

#[tokio::test]
async fn models_kind_filter_and_info_endpoint() {
    let _g = lock();
    let (_d, router, _db) = app_with(false).await;

    // `image` is a known kind slug, not a model id: it lists image models only.
    let (s, body) = call(router.clone(), "GET", "/v1/models/image", None, None).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let image_data = body["data"].as_array().unwrap();
    assert!(!image_data.is_empty(), "the catalog has image models");
    // Every listed model must belong to a configured provider kind, not be an
    // llm: spot-check that a known text model is absent.
    let ids: Vec<&str> = image_data.iter().filter_map(|m| m["id"].as_str()).collect();
    assert!(!ids.iter().any(|id| *id == "openai/gpt-4o"), "llm leaked into the image list: {ids:?}");
    for id in &ids {
        assert!(id.contains('/'), "{id}");
    }

    // Missing `id` query is a 400.
    let (s, body) = call(router.clone(), "GET", "/v1/models/info", None, None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");

    // A known provider/model resolves to metadata with a kind.
    let (s, body) = call(router.clone(), "GET", "/v1/models/info?id=openai/gpt-4o", None, None).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["id"], "openai/gpt-4o");
    assert_eq!(body["owned_by"], "openai");
    assert!(body["kind"].is_string(), "{body}");

    // info without a provider prefix is a 404.
    let (s, _) = call(router.clone(), "GET", "/v1/models/info?id=bare-model", None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

// ─── /v1/chat/completions ───────────────────────────────────────────────────

#[tokio::test]
async fn chat_without_key_is_401() {
    let _g = lock();
    let (_d, router, _db) = app_with(true).await;
    let body = json!({"model": "openai/gpt-4o", "messages": []});
    let (s, body) = call(router.clone(), "POST", "/v1/chat/completions", Some(body), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"]["message"], "Missing API key");
    // Same story for the messages endpoint.
    let (s, _) = call(router.clone(), "POST", "/v1/messages", Some(body), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn chat_routes_dispatch_to_engine() {
    let _g = lock();
    let (_d, router, _db) = app_with(false).await;
    let mock = install(MockEngine::json(200, &openai_ok_body()));

    let body = json!({
        "model": "openai/gpt-4o",
        "messages": [{"role": "user", "content": "hi"}],
        "stream": false
    });
    let (s, out) = call(router.clone(), "POST", "/v1/chat/completions", Some(body), None).await;
    assert_eq!(s, StatusCode::OK, "{out}");
    assert_eq!(out["object"], "chat.completion");
    assert_eq!(out["choices"][0]["message"]["content"], "hi there");
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);

    let seen = mock.last.lock().unwrap().clone().expect("engine called");
    assert_eq!(seen.endpoint, "/v1/chat/completions");
    assert_eq!(seen.model, "openai/gpt-4o");
    drop(seen);

    // Claude-compatible endpoint keeps its own endpoint label.
    let body = json!({"model": "anthropic/claude-sonnet-4", "messages": [{"role": "user", "content": "hi"}]});
    let (s, _) = call(router.clone(), "POST", "/v1/messages", Some(body), None).await;
    assert_eq!(s, StatusCode::OK);
    let seen = mock.last.lock().unwrap().clone().expect("engine called");
    assert_eq!(seen.endpoint, "/v1/messages");
    assert_eq!(seen.model, "anthropic/claude-sonnet-4");
}

#[tokio::test]
async fn chat_missing_model_is_400() {
    let _g = lock();
    let (_d, router, _db) = app_with(false).await;
    install(MockEngine::json(200, &openai_ok_body()));

    let (s, body) = call(router.clone(), "POST", "/v1/chat/completions", Some(json!({"messages": []})), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["message"], "Missing model");
    assert_eq!(body["error"]["type"], "invalid_request_error");

    // A blank model string is also rejected, not forwarded.
    let (s, _) = call(router.clone(), "POST", "/v1/chat/completions", Some(json!({"model": "  ", "messages": []})), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn chat_engine_error_is_passed_through_with_envelope() {
    let _g = lock();
    let (_d, router, _db) = app_with(false).await;
    install(MockEngine::err(429, "rate limited upstream"));
    let body = json!({"model": "openai/gpt-4o", "messages": []});
    let (s, out) = call(router.clone(), "POST", "/v1/chat/completions", Some(body), None).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "{out}");
    assert_eq!(out["error"]["type"], "rate_limit_error");
    assert!(out["error"]["message"].as_str().unwrap().contains("rate limited"));
}

#[tokio::test]
async fn chat_non_streaming_returns_json_content_type() {
    let _g = lock();
    let (_d, router, _db) = app_with(false).await;
    install(MockEngine::json(200, &openai_ok_body()));
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"model": "openai/gpt-4o", "messages": [], "stream": false}).to_string()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()),
        Some("application/json"),
        "non-streaming chat answers with JSON"
    );
    // CORS is set on OPTIONS and on error responses only, matching the JS
    // route (src/app/api/v1/chat/completions/route.js OPTIONS handler).
    let preflight = router
        .clone()
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/v1/chat/completions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        preflight.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
        Some(&HeaderValue::from_static("*"))
    );
}

#[tokio::test]
async fn chat_malformed_body_is_400() {
    let _g = lock();
    let (_d, router, _db) = app_with(false).await;
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{ not json"))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ─── /v1/messages/count_tokens ──────────────────────────────────────────────

/// The estimator walks every JSON string/number leaf, including object keys and
/// the `role` field, then divides the character total by 4 (rounding up).
#[tokio::test]
async fn count_tokens_counts_system_and_messages() {
    let _g = lock();
    let (_d, router, _db) = app_with(false).await;

    // No key guard on this endpoint: it is a pure character estimate.
    let body = json!({
        "system": "You are a helpful assistant",
        "messages": [{ "role": "user", "content": "hello world" }]
    });
    let (s, out) = call(router.clone(), "POST", "/v1/messages/count_tokens", Some(body.clone()), None).await;
    assert_eq!(s, StatusCode::OK, "{out}");

    // Reproduce the documented rule: system + tools + each message is walked
    // (keys included), total chars + 3, then /4 rounded down.
    let chars = count_value_chars(&body.get("system").unwrap_or(&Value::Null))
        + body.get("messages").and_then(Value::as_array).map(|a| a.iter().map(count_value_chars).sum()).unwrap_or(0);
    assert_eq!(out["input_tokens"], json!((chars + 3) / 4));

    // A bigger system prompt must strictly increase the estimate.
    let bigger = json!({
        "system": "You are a helpful assistant. Always answer in French.",
        "messages": [{ "role": "user", "content": "hello world" }]
    });
    let (_, out2) = call(router.clone(), "POST", "/v1/messages/count_tokens", Some(bigger), None).await;
    assert!(out2["input_tokens"].as_u64() > out["input_tokens"].as_u64(), "{out2} vs {out}");

    // Empty body still works and never under-flows.
    let (s, out) = call(router.clone(), "POST", "/v1/messages/count_tokens", Some(json!({})), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(out["input_tokens"], 0);

    let (s, _) = call(router.clone(), "POST", "/v1/messages/count_tokens", None, None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

/// Mirrors `count_value_chars` in the route: strings, numbers, and object keys.
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

// ─── /v1beta ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn v1beta_models_gemini_shape() {
    let _g = lock();
    let (_d, router, _db) = app_with(false).await;
    let (s, body) = call(router.clone(), "GET", "/v1beta/models", None, None).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    for m in body["models"].as_array().unwrap() {
        assert!(m["name"].as_str().unwrap().starts_with("models/"));
        assert!(m["supportedGenerationMethods"].as_array().is_some());
    }
    // gemini models get both the provider-prefixed and bare-name listing.
    let names: Vec<&str> = body["models"].as_array().unwrap().iter().filter_map(|m| m["name"].as_str()).collect();
    assert!(names.iter().any(|n| n.starts_with("models/gemini/")), "{names:?}");
}

/// BUG(nr-server): `/v1beta/models/{provider}/{model}:generateContent` does not
/// match the Rust route table (only a single `{model}` segment is registered),
/// so the JS-documented `provider/model` form 404s. JS handles it at
/// src/app/api/v1beta/models/[...path]/route.js (`path.length >= 2`).
#[tokio::test]
async fn v1beta_generate_maps_gemini_body_and_reports_auth() {
    let _g = lock();
    let (_d, router, _db) = app_with(true).await;
    // Auth guard applies here as well.
    let body = json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
    let (s, out) = call(router.clone(), "POST", "/v1beta/models/openai/gpt-4o:generateContent", Some(body), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{out}");
}

/// See the note on [`v1beta_generate_maps_gemini_body_and_reports_auth`].
#[tokio::test]
async fn v1beta_generate_dispatches_translated_body() {
    let _g = lock();
    let (_d, router, _db) = app_with(false).await;
    let mock = install(MockEngine::json(200, &json!({"candidates": []})));
    let body = json!({
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "systemInstruction": { "parts": [{ "text": "be terse" }] }
    });
    let (s, _) = call(router.clone(), "POST", "/v1beta/models/openai/gpt-4o:generateContent", Some(body), None).await;
    assert_eq!(s, StatusCode::OK);
    let seen = mock.last.lock().unwrap().clone().expect("engine called");
    assert_eq!(seen.model, "openai/gpt-4o");
    assert!(!seen.stream, "generateContent is non-streaming");
    assert_eq!(seen.body["messages"], json!([{"role": "user", "parts": [{"text": "hi"}]}]));
    assert_eq!(seen.body["system"], json!([{"text": "be terse"}]));
    drop(seen);

    // The `:streamGenerateContent` action decides streaming, not a body field.
    let body = json!({"contents": []});
    let (s, _) = call(router.clone(), "POST", "/v1beta/models/openai/gpt-4o:streamGenerateContent", Some(body), None).await;
    assert_eq!(s, StatusCode::OK);
    let seen = mock.last.lock().unwrap().clone().expect("engine called");
    assert!(seen.stream);
}

/// See the note on [`v1beta_generate_maps_gemini_body_and_reports_auth`].
#[tokio::test]
async fn v1beta_rejects_non_json_body() {
    let _g = lock();
    let (_d, router, _db) = app_with(false).await;
    let req = Request::builder()
        .method("POST")
        .uri("/v1beta/models/openai/gpt-4o:generateContent")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("nope"))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ─── Per-key permissions + request logging (port of requestLog.js) ──────────

#[tokio::test]
async fn restricted_key_is_denied_outside_its_allow_lists() {
    let _g = lock();
    let (_d, router, db) = app_with(true).await;
    let mock = install(MockEngine::json(200, &openai_ok_body()));

    let key = nr_db::create_api_key(&db, Some("Team A".into()), "machine-1".into(), None).unwrap();
    // `allowedModels: ["cc/*"]`, `allowedEndpoints: ["chat"]`
    db.execute_with_params(
        "UPDATE apiKeys SET allowedModels = ?, allowedEndpoints = ? WHERE id = ?",
        (
            json!(["cc/*"]).to_string(),
            json!(["chat"]).to_string(),
            key.id.clone(),
        ),
    )
    .unwrap();

    // Model outside the allow list → 403, engine never called.
    let body = json!({"model": "gh/gpt-5", "messages": []});
    let (s, out) = call(router.clone(), "POST", "/v1/chat/completions", Some(body), Some(&key.key)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{out}");
    assert_eq!(out["error"]["message"], "API key not allowed to use model: gh/gpt-5");
    assert_eq!(mock.calls.load(Ordering::SeqCst), 0);

    // Allowed model still passes.
    let body = json!({"model": "cc/sonnet", "messages": []});
    let (s, out) = call(router.clone(), "POST", "/v1/chat/completions", Some(body), Some(&key.key)).await;
    assert_eq!(s, StatusCode::OK, "{out}");
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);

    // A non-chat endpoint is denied for the same key (embeddings route).
    let (s, out) = call(
        router.clone(),
        "POST",
        "/v1/embeddings",
        Some(json!({"model": "cc/sonnet", "input": "hi"})),
        Some(&key.key),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{out}");
    assert!(out["error"]["message"].as_str().unwrap().contains("endpoint type"));
}

#[tokio::test]
async fn chat_request_writes_a_request_log_row() {
    let _g = lock();
    let (_d, router, db) = app_with(true).await;
    install(MockEngine::json(200, &openai_ok_body()));

    let key = nr_db::create_api_key(&db, Some("Team A".into()), "machine-1".into(), None).unwrap();
    let body = json!({"model": "openai/gpt-4o", "messages": [{"role":"user","content":"hi"}], "stream": false});
    let (s, out) = call(router.clone(), "POST", "/v1/chat/completions", Some(body), Some(&key.key)).await;
    assert_eq!(s, StatusCode::OK, "{out}");

    let logs = nr_db::get_request_logs(&db, &nr_db::RequestLogFilter::default()).unwrap();
    let rows = logs["logs"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{logs}");
    let row = &rows[0];
    assert_eq!(row["status"], json!(200));
    assert_eq!(row["endpointKind"], json!("chat"));
    assert_eq!(row["path"], json!("/v1/chat/completions"));
    assert_eq!(row["model"], json!("openai/gpt-4o"));
    assert_eq!(row["apiKeyId"], json!(key.id));
    assert_eq!(row["apiKeyName"], json!("Team A"));
    assert_eq!(row["method"], json!("POST"));
    assert_eq!(row["stream"], json!(false));
}

#[tokio::test]
async fn denied_request_is_logged_with_403() {
    let _g = lock();
    let (_d, router, db) = app_with(true).await;
    install(MockEngine::json(200, &openai_ok_body()));

    let key = nr_db::create_api_key(&db, Some("Denied".into()), "machine-1".into(), None).unwrap();
    db.execute_with_params(
        "UPDATE apiKeys SET allowedModels = ? WHERE id = ?",
        (json!(["cc/*"]).to_string(), key.id.clone()),
    )
    .unwrap();

    let body = json!({"model": "gh/gpt-5", "messages": []});
    let (s, _) = call(router.clone(), "POST", "/v1/chat/completions", Some(body), Some(&key.key)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    let logs = nr_db::get_request_logs(&db, &nr_db::RequestLogFilter::default()).unwrap();
    assert_eq!(logs["logs"][0]["status"], json!(403));
}

#[tokio::test]
async fn rpm_limit_returns_429_with_retry_after() {
    let _g = lock();
    let (_d, router, db) = app_with(true).await;
    install(MockEngine::json(200, &openai_ok_body()));

    let key = nr_db::create_api_key(&db, Some("Limited".into()), "machine-1".into(), None).unwrap();
    db.execute_with_params("UPDATE apiKeys SET rpm = 1 WHERE id = ?", [key.id.clone()])
        .unwrap();

    let body = json!({"model": "openai/gpt-4o", "messages": []});
    let (s, _) = call(router.clone(), "POST", "/v1/chat/completions", Some(body.clone()), Some(&key.key)).await;
    assert_eq!(s, StatusCode::OK);

    let resp = {
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("authorization", format!("Bearer {}", key.key))
            .header(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .body(Body::from(body.to_string()))
            .unwrap();
        router.oneshot(req).await.unwrap()
    };
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .expect("Retry-After header");
    assert!(retry_after.parse::<i64>().unwrap() >= 1);
}
