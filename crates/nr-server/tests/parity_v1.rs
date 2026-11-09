//! HTTP parity tests for `/v1` and `/v1beta` against `nr_server::app()`.
//!
//! Port of `../9router/src/app/api/v1/**` and `../9router/src/app/api/v1beta/**`.
//! Exercises axum oneshot requests against the full `nr_server::app()` router:
//! - POST /v1/chat/completions (streaming true & false; valid, invalid, missing key)
//! - POST /v1/messages (Anthropic format compatibility)
//! - POST /v1/responses and POST /v1/responses/compact
//! - GET /v1/models, GET /v1/models/info, GET /v1/models/{*model}
//! - POST /v1/embeddings
//! - POST /v1/images/generations
//! - POST /v1/audio/speech and POST /v1/audio/transcriptions
//! - POST /v1/search
//! - POST /v1/web/fetch
//! - POST /v1/systemone
//! - GET /v1beta/models and /v1beta/models/{*path}
//!
//! Verifies that JSON error formats strictly conform to:
//! `{ "error": { "message": "...", "type": "...", "code": "..." } }`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, Method, Request, StatusCode};
use bytes::Bytes;
use nr_core::http::UpstreamResponse;
use nr_core::{json, Result};
use nr_db::repos::{create_provider_connection, get_api_key_by_key, update_settings, ApiKey};
use nr_db::Db;
use nr_engine::{
    set_chat_engine, set_embeddings_engine, set_images_engine, ChatEngine, ChatRequest,
    PassThroughEmbeddingsEngine, PassThroughImagesEngine,
};
use serde_json::Value;
use tempfile::TempDir;
use tower::ServiceExt;

// ─── Test Harness & Mock Engine ─────────────────────────────────────────────

struct MockChatEngine {
    next: Box<dyn Fn(ChatRequest) -> Result<UpstreamResponse> + Send + Sync>,
    last: Mutex<Option<ChatRequest>>,
    calls: AtomicUsize,
}

impl MockChatEngine {
    fn json(status: u16, body: &Value) -> Self {
        let body = body.clone();
        Self {
            next: Box::new(move |_| Ok(UpstreamResponse::json(status, &body))),
            last: Mutex::new(None),
            calls: AtomicUsize::new(0),
        }
    }

    fn sse(chunks: Vec<String>) -> Self {
        Self {
            next: Box::new(move |_| {
                let items: Vec<std::io::Result<Bytes>> = chunks.iter().map(|c| Ok(Bytes::from(c.clone()))).collect();
                let stream = Box::pin(futures::stream::iter(items));
                Ok(UpstreamResponse::sse(stream))
            }),
            last: Mutex::new(None),
            calls: AtomicUsize::new(0),
        }
    }

    fn err(status: u16, message: &str) -> Self {
        let msg = message.to_string();
        Self {
            next: Box::new(move |_| Err(nr_core::Error::Upstream { status, message: msg.clone() })),
            last: Mutex::new(None),
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl ChatEngine for MockChatEngine {
    async fn handle_chat(&self, req: ChatRequest) -> Result<UpstreamResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last.lock().unwrap() = Some(req.clone());
        (self.next)(req)
    }
}

static TEST_DIR: OnceLock<String> = OnceLock::new();
static GLOBAL_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    GLOBAL_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

fn init_test_env() -> (Db, ApiKey) {
    TEST_DIR.get_or_init(|| {
        let dir = TempDir::new().expect("create temp dir");
        let path = dir.path().to_path_buf();
        std::mem::forget(dir);
        std::env::set_var("DATA_DIR", &path);
        path.display().to_string()
    });
    let db = nr_db::db().expect("open test db");
    // Ensure requireApiKey is true by default
    update_settings(&db, json!({ "requireApiKey": true })).expect("set requireApiKey");

    // Ensure OpenAI provider connection exists for model tests
    let _ = create_provider_connection(
        &db,
        &json!({
            "id": "parity-conn-openai",
            "provider": "openai",
            "authType": "apikey",
            "name": "parity-openai",
            "priority": 1,
            "isActive": true,
            "providerSpecificData": { "apiKey": "sk-mock-key" }
        }),
    );

    let key_name = "parity-test-key";
    let key = match get_api_key_by_key(&db, "sk-parity-valid-key-123456789012") {
        Ok(Some(k)) => k,
        _ => {
            let id = "parity-key-id-1";
            let key_str = "sk-parity-valid-key-123456789012";
            let now = chrono::Utc::now().to_rfc3339();
            let _ = db.execute_with_params(
                "INSERT OR REPLACE INTO apiKeys(id, key, name, machineId, isActive, createdAt, accessRestricted, accessAllow) VALUES(?, ?, ?, ?, 1, ?, 0, '[]')",
                [id, key_str, key_name, "parity-machine", &now],
            );
            get_api_key_by_key(&db, key_str).unwrap().expect("valid key created")
        }
    };

    (db, key)
}

fn app_with_chat(mock: Arc<MockChatEngine>) -> axum::Router {
    let app = nr_server::app();
    set_chat_engine(mock);
    set_embeddings_engine(Arc::new(PassThroughEmbeddingsEngine));
    set_images_engine(Arc::new(PassThroughImagesEngine));
    app
}

fn app_default() -> axum::Router {
    let app = nr_server::app();
    set_embeddings_engine(Arc::new(PassThroughEmbeddingsEngine));
    set_images_engine(Arc::new(PassThroughImagesEngine));
    app
}

async fn oneshot_request(
    router: axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    auth: Option<&str>,
) -> (StatusCode, HeaderMap, Value, Bytes) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = auth {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let req = match body {
        Some(b) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), 10_000_000).await.unwrap();
    let val = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, headers, val, bytes)
}

async fn oneshot_raw(
    router: axum::Router,
    method: &str,
    uri: &str,
    raw_body: Option<&str>,
    content_type: Option<&str>,
    auth: Option<&str>,
) -> (StatusCode, HeaderMap, Value, Bytes) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = auth {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(ct) = content_type {
        builder = builder.header(header::CONTENT_TYPE, ct);
    }
    let req = match raw_body {
        Some(b) => builder.body(Body::from(b.to_string())).unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), 10_000_000).await.unwrap();
    let val = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, headers, val, bytes)
}

/// Strictly verify that JSON error responses conform to:
/// `{ "error": { "message": "...", "type": "...", "code": "..." } }`.
fn assert_error_envelope(status: StatusCode, body: &Value, expected_status: StatusCode) {
    assert_eq!(status, expected_status, "expected status {expected_status}, got {status}: {body}");
    let error = body.get("error").unwrap_or_else(|| panic!("body must contain 'error' object, got: {body}"));
    assert!(error.is_object(), "'error' field must be a JSON object, got: {error}");
    let message = error.get("message").and_then(Value::as_str).unwrap_or_else(|| panic!("'error.message' must be a string, got: {error}"));
    assert!(!message.is_empty(), "'error.message' must not be empty, got: {error}");
    let err_type = error.get("type").and_then(Value::as_str).unwrap_or_else(|| panic!("'error.type' must be a string, got: {error}"));
    assert!(!err_type.is_empty(), "'error.type' must not be empty, got: {error}");
    let code = error.get("code").and_then(Value::as_str).unwrap_or_else(|| panic!("'error.code' must be a string, got: {error}"));
    let _ = code; // string exists (empty or specific code)
}

fn sample_openai_chat_response() -> Value {
    json!({
        "id": "chatcmpl-test-1",
        "object": "chat.completion",
        "model": "openai/gpt-4o",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "Hello, world!" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 8, "completion_tokens": 4, "total_tokens": 12 }
    })
}

// ─── 1. POST /v1/chat/completions ───────────────────────────────────────────

#[tokio::test]
async fn test_chat_completions_missing_key() {
    let _g = lock();
    let (_db, _key) = init_test_env();
    let router = app_default();

    let body = json!({
        "model": "openai/gpt-4o",
        "messages": [{ "role": "user", "content": "hi" }]
    });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/chat/completions", Some(body), None).await;
    assert_error_envelope(status, &json_body, StatusCode::UNAUTHORIZED);
    assert_eq!(json_body["error"]["message"], "Missing API key");
    assert_eq!(json_body["error"]["type"], "authentication_error");
    assert_eq!(json_body["error"]["code"], "invalid_api_key");
}

#[tokio::test]
async fn test_chat_completions_invalid_key() {
    let _g = lock();
    let (_db, _key) = init_test_env();
    let router = app_default();

    let body = json!({
        "model": "openai/gpt-4o",
        "messages": [{ "role": "user", "content": "hi" }]
    });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/chat/completions", Some(body), Some("sk-invalid-nonexistent")).await;
    assert_error_envelope(status, &json_body, StatusCode::UNAUTHORIZED);
    assert_eq!(json_body["error"]["message"], "Invalid API key");
    assert_eq!(json_body["error"]["type"], "authentication_error");
    assert_eq!(json_body["error"]["code"], "invalid_api_key");
}

#[tokio::test]
async fn test_chat_completions_non_streaming_valid_key() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let mock = Arc::new(MockChatEngine::json(200, &sample_openai_chat_response()));
    let router = app_with_chat(mock.clone());

    let body = json!({
        "model": "openai/gpt-4o",
        "messages": [{ "role": "user", "content": "hello" }],
        "stream": false
    });
    let (status, headers, json_body, _bytes) = oneshot_request(router, "POST", "/v1/chat/completions", Some(body), Some(&key.key)).await;
    assert_eq!(status, StatusCode::OK, "{json_body}");
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    assert!(ct.contains("application/json"), "expected json content type, got: {ct}");
    assert_eq!(json_body["object"], "chat.completion");
    assert_eq!(json_body["choices"][0]["message"]["content"], "Hello, world!");

    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    let seen = mock.last.lock().unwrap().clone().expect("engine called");
    assert_eq!(seen.endpoint, "/v1/chat/completions");
    assert_eq!(seen.model, "openai/gpt-4o");
    assert!(!seen.stream, "streaming should be false");
}

#[tokio::test]
async fn test_chat_completions_streaming_valid_key() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let chunks = vec![
        "data: {\"id\":\"chatcmpl-test-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"}}]}\n\n".to_string(),
        "data: {\"id\":\"chatcmpl-test-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"}}]}\n\n".to_string(),
        "data: [DONE]\n\n".to_string(),
    ];
    let mock = Arc::new(MockChatEngine::sse(chunks));
    let router = app_with_chat(mock.clone());

    let body = json!({
        "model": "openai/gpt-4o",
        "messages": [{ "role": "user", "content": "stream to me" }],
        "stream": true
    });
    let (status, headers, _json_body, bytes) = oneshot_request(router, "POST", "/v1/chat/completions", Some(body), Some(&key.key)).await;
    assert_eq!(status, StatusCode::OK);
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    assert!(ct.contains("text/event-stream"), "expected text/event-stream, got: {ct}");

    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("chat.completion.chunk"), "stream should contain chunk: {text}");
    assert!(text.contains("[DONE]"), "stream should contain [DONE]: {text}");

    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    let seen = mock.last.lock().unwrap().clone().expect("engine called");
    assert_eq!(seen.endpoint, "/v1/chat/completions");
    assert!(seen.stream, "streaming should be true");
}

#[tokio::test]
async fn test_chat_completions_missing_model() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let body = json!({ "messages": [{ "role": "user", "content": "hi" }] });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/chat/completions", Some(body), Some(&key.key)).await;
    assert_error_envelope(status, &json_body, StatusCode::BAD_REQUEST);
    assert_eq!(json_body["error"]["message"], "Missing model");
}

#[tokio::test]
async fn test_chat_completions_empty_body() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/chat/completions", None, Some(&key.key)).await;
    assert_error_envelope(status, &json_body, StatusCode::BAD_REQUEST);
    assert_eq!(json_body["error"]["message"], "Invalid JSON body");
}

#[tokio::test]
async fn test_chat_completions_malformed_json() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let (status, _hdrs, _json_body, _bytes) = oneshot_raw(
        router,
        "POST",
        "/v1/chat/completions",
        Some("{ bad json:"),
        Some("application/json"),
        Some(&key.key),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ─── 2. POST /v1/messages (Anthropic format compatibility) ──────────────────

#[tokio::test]
async fn test_messages_missing_key() {
    let _g = lock();
    let (_db, _key) = init_test_env();
    let router = app_default();

    let body = json!({
        "model": "anthropic/claude-3-5-sonnet",
        "messages": [{ "role": "user", "content": "hi" }]
    });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/messages", Some(body), None).await;
    assert_error_envelope(status, &json_body, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_messages_invalid_key() {
    let _g = lock();
    let (_db, _key) = init_test_env();
    let router = app_default();

    let body = json!({
        "model": "anthropic/claude-3-5-sonnet",
        "messages": [{ "role": "user", "content": "hi" }]
    });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/messages", Some(body), Some("sk-nope")).await;
    assert_error_envelope(status, &json_body, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_messages_valid_key() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let anthropic_response = json!({
        "id": "msg_01XyZ",
        "type": "message",
        "role": "assistant",
        "content": [{ "type": "text", "text": "Anthropic response message" }],
        "model": "anthropic/claude-3-5-sonnet",
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 10, "output_tokens": 6 }
    });
    let mock = Arc::new(MockChatEngine::json(200, &anthropic_response));
    let router = app_with_chat(mock.clone());

    let body = json!({
        "model": "anthropic/claude-3-5-sonnet",
        "messages": [{ "role": "user", "content": "hello claude" }]
    });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/messages", Some(body), Some(&key.key)).await;
    assert_eq!(status, StatusCode::OK, "{json_body}");
    assert_eq!(json_body["type"], "message");
    assert_eq!(json_body["role"], "assistant");

    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    let seen = mock.last.lock().unwrap().clone().expect("engine called");
    assert_eq!(seen.endpoint, "/v1/messages");
    assert_eq!(seen.model, "anthropic/claude-3-5-sonnet");
}

#[tokio::test]
async fn test_messages_missing_model() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let body = json!({ "messages": [{ "role": "user", "content": "hi" }] });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/messages", Some(body), Some(&key.key)).await;
    assert_error_envelope(status, &json_body, StatusCode::BAD_REQUEST);
    assert_eq!(json_body["error"]["message"], "Missing model");
}

// ─── 3. POST /v1/responses & POST /v1/responses/compact ─────────────────────

#[tokio::test]
async fn test_responses_missing_key() {
    let _g = lock();
    let (_db, _key) = init_test_env();
    let router = app_default();

    let body = json!({ "model": "openai/gpt-4o", "input": "test prompt" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/responses", Some(body), None).await;
    assert_error_envelope(status, &json_body, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_responses_invalid_key() {
    let _g = lock();
    let (_db, _key) = init_test_env();
    let router = app_default();

    let body = json!({ "model": "openai/gpt-4o", "input": "test prompt" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/responses", Some(body), Some("sk-invalid")).await;
    assert_error_envelope(status, &json_body, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_responses_valid_key() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let mock = Arc::new(MockChatEngine::json(200, &sample_openai_chat_response()));
    let router = app_with_chat(mock.clone());

    let body = json!({ "model": "openai/gpt-4o", "input": "hello responses" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/responses", Some(body), Some(&key.key)).await;
    assert_eq!(status, StatusCode::OK, "{json_body}");
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    let seen = mock.last.lock().unwrap().clone().expect("engine called");
    assert_eq!(seen.endpoint, "/v1/responses");
    assert_eq!(seen.model, "openai/gpt-4o");
}

#[tokio::test]
async fn test_responses_compact_valid_key() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let mock = Arc::new(MockChatEngine::json(200, &sample_openai_chat_response()));
    let router = app_with_chat(mock.clone());

    let body = json!({ "model": "openai/gpt-4o", "input": "hello compact" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/responses/compact", Some(body), Some(&key.key)).await;
    assert_eq!(status, StatusCode::OK, "{json_body}");
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    let seen = mock.last.lock().unwrap().clone().expect("engine called");
    assert_eq!(seen.endpoint, "/v1/responses");
    assert_eq!(seen.model, "openai/gpt-4o");
}

#[tokio::test]
async fn test_responses_missing_model() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let body = json!({ "input": "no model" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/responses", Some(body), Some(&key.key)).await;
    assert_error_envelope(status, &json_body, StatusCode::BAD_REQUEST);
    assert_eq!(json_body["error"]["message"], "Missing model");
}

// ─── 4. GET /v1/models, GET /v1/models/info, GET /v1/models/{*model} ────────

#[tokio::test]
async fn test_models_list_missing_key() {
    let _g = lock();
    let (_db, _key) = init_test_env();
    let router = app_default();

    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "GET", "/v1/models", None, None).await;
    assert_error_envelope(status, &json_body, StatusCode::UNAUTHORIZED);
    assert_eq!(json_body["error"]["message"], "Missing API key");
}

#[tokio::test]
async fn test_models_list_invalid_key() {
    let _g = lock();
    let (_db, _key) = init_test_env();
    let router = app_default();

    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "GET", "/v1/models", None, Some("sk-invalid")).await;
    assert_error_envelope(status, &json_body, StatusCode::UNAUTHORIZED);
    assert_eq!(json_body["error"]["message"], "Invalid API key");
}

#[tokio::test]
async fn test_models_list_valid_key() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "GET", "/v1/models", None, Some(&key.key)).await;
    assert_eq!(status, StatusCode::OK, "{json_body}");
    assert_eq!(json_body["object"], "list");
    let data = json_body["data"].as_array().expect("data array");
    assert!(!data.is_empty(), "models array should not be empty");
    for m in data {
        assert_eq!(m["object"], "model");
        assert!(m["id"].as_str().is_some());
        assert!(m["owned_by"].as_str().is_some());
    }
}

#[tokio::test]
async fn test_models_info_missing_param() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "GET", "/v1/models/info", None, Some(&key.key)).await;
    assert_error_envelope(status, &json_body, StatusCode::BAD_REQUEST);
    assert!(json_body["error"]["message"].as_str().unwrap().contains("Missing required query param: id"));
}

#[tokio::test]
async fn test_models_info_valid_model() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "GET", "/v1/models/info?id=openai/gpt-4o", None, Some(&key.key)).await;
    assert_eq!(status, StatusCode::OK, "{json_body}");
    assert_eq!(json_body["id"], "openai/gpt-4o");
    assert_eq!(json_body["owned_by"], "openai");
    assert!(json_body["kind"].is_string());
}

#[tokio::test]
async fn test_models_info_not_found() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "GET", "/v1/models/info?id=openai/nonexistent-xyz-99", None, Some(&key.key)).await;
    assert_error_envelope(status, &json_body, StatusCode::NOT_FOUND);
    assert!(json_body["error"]["message"].as_str().unwrap().contains("Model not found"));
}

#[tokio::test]
async fn test_models_by_kind_slug() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "GET", "/v1/models/image", None, Some(&key.key)).await;
    assert_eq!(status, StatusCode::OK, "{json_body}");
    assert_eq!(json_body["object"], "list");
    let list = json_body["data"].as_array().expect("data array");
    assert!(!list.is_empty(), "image models should exist in catalog");
    for item in list {
        assert_eq!(item["object"], "model");
        assert!(item["id"].as_str().unwrap().contains('/'));
    }
}

#[tokio::test]
async fn test_models_single_model_by_id() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "GET", "/v1/models/openai/gpt-4o", None, Some(&key.key)).await;
    assert_eq!(status, StatusCode::OK, "{json_body}");
    assert_eq!(json_body["id"], "openai/gpt-4o");
    assert_eq!(json_body["object"], "model");
}

#[tokio::test]
async fn test_models_single_model_not_found() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "GET", "/v1/models/openai/nonexistent-xyz-888", None, Some(&key.key)).await;
    assert_error_envelope(status, &json_body, StatusCode::NOT_FOUND);
    assert!(json_body["error"]["message"].as_str().unwrap().contains("does not exist"));
}

// ─── 5. POST /v1/embeddings ─────────────────────────────────────────────────

#[tokio::test]
async fn test_embeddings_missing_key() {
    let _g = lock();
    let (_db, _key) = init_test_env();
    let router = app_default();

    let body = json!({ "model": "text-embedding-3-small", "input": "hello" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/embeddings", Some(body), None).await;
    assert_error_envelope(status, &json_body, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_embeddings_invalid_key() {
    let _g = lock();
    let (_db, _key) = init_test_env();
    let router = app_default();

    let body = json!({ "model": "text-embedding-3-small", "input": "hello" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/embeddings", Some(body), Some("sk-invalid")).await;
    assert_error_envelope(status, &json_body, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_embeddings_missing_model() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let body = json!({ "input": "hello" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/embeddings", Some(body), Some(&key.key)).await;
    assert_error_envelope(status, &json_body, StatusCode::BAD_REQUEST);
    assert_eq!(json_body["error"]["message"], "Missing model");
}

#[tokio::test]
async fn test_embeddings_missing_input() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let body = json!({ "model": "text-embedding-3-small" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/embeddings", Some(body), Some(&key.key)).await;
    assert_error_envelope(status, &json_body, StatusCode::BAD_REQUEST);
    assert_eq!(json_body["error"]["message"], "Missing required field: input");
}

#[tokio::test]
async fn test_embeddings_valid_request() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let body = json!({
        "model": "text-embedding-3-small",
        "input": "test embedding input"
    });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/embeddings", Some(body), Some(&key.key)).await;
    assert_eq!(status, StatusCode::OK, "{json_body}");
    assert_eq!(json_body["object"], "list");
    let data = json_body["data"].as_array().expect("data array");
    assert!(!data.is_empty());
    assert_eq!(data[0]["object"], "embedding");
    assert!(data[0]["embedding"].as_array().is_some());
}

// ─── 6. POST /v1/images/generations ─────────────────────────────────────────

#[tokio::test]
async fn test_images_missing_key() {
    let _g = lock();
    let (_db, _key) = init_test_env();
    let router = app_default();

    let body = json!({ "prompt": "a sunset over mountains", "model": "dall-e-3" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/images/generations", Some(body), None).await;
    assert_error_envelope(status, &json_body, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_images_invalid_key() {
    let _g = lock();
    let (_db, _key) = init_test_env();
    let router = app_default();

    let body = json!({ "prompt": "a sunset over mountains", "model": "dall-e-3" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/images/generations", Some(body), Some("sk-invalid")).await;
    assert_error_envelope(status, &json_body, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_images_empty_body() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/images/generations", None, Some(&key.key)).await;
    assert_error_envelope(status, &json_body, StatusCode::BAD_REQUEST);
    assert_eq!(json_body["error"]["message"], "Invalid JSON body");
}

#[tokio::test]
async fn test_images_malformed_json() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let (status, _hdrs, _json_body, _bytes) = oneshot_raw(
        router,
        "POST",
        "/v1/images/generations",
        Some("not json"),
        Some("application/json"),
        Some(&key.key),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_images_valid_request() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let body = json!({ "prompt": "cute cat in a basket", "model": "dall-e-3" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/images/generations", Some(body), Some(&key.key)).await;
    assert_eq!(status, StatusCode::OK, "{json_body}");
    let data = json_body["data"].as_array().expect("data array");
    assert!(!data.is_empty());
    assert!(data[0]["url"].is_string());
}

// ─── 7. POST /v1/audio/speech & POST /v1/audio/transcriptions ───────────────

#[tokio::test]
async fn test_audio_speech_missing_key() {
    let _g = lock();
    let (_db, _key) = init_test_env();
    let router = app_default();

    let body = json!({ "model": "tts-1", "input": "Hello", "voice": "alloy" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/audio/speech", Some(body), None).await;
    assert_error_envelope(status, &json_body, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_audio_speech_invalid_key() {
    let _g = lock();
    let (_db, _key) = init_test_env();
    let router = app_default();

    let body = json!({ "model": "tts-1", "input": "Hello", "voice": "alloy" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/audio/speech", Some(body), Some("sk-invalid")).await;
    assert_error_envelope(status, &json_body, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_audio_speech_valid_request() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let body = json!({ "model": "tts-1", "input": "Hello, speech test", "voice": "alloy" });
    let (status, headers, _json_body, bytes) = oneshot_request(router, "POST", "/v1/audio/speech", Some(body), Some(&key.key)).await;
    assert_eq!(status, StatusCode::OK);
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    assert!(ct.contains("audio/mpeg"), "expected audio/mpeg, got: {ct}");
    assert_eq!(&bytes[..], b"ID3mockaudiobytes");
}

#[tokio::test]
async fn test_audio_transcriptions_missing_key() {
    let _g = lock();
    let (_db, _key) = init_test_env();
    let router = app_default();

    let body = json!({ "model": "whisper-1" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/audio/transcriptions", Some(body), None).await;
    assert_error_envelope(status, &json_body, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_audio_transcriptions_invalid_key() {
    let _g = lock();
    let (_db, _key) = init_test_env();
    let router = app_default();

    let body = json!({ "model": "whisper-1" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/audio/transcriptions", Some(body), Some("sk-invalid")).await;
    assert_error_envelope(status, &json_body, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_audio_transcriptions_valid_request() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let body = json!({ "model": "whisper-1" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/audio/transcriptions", Some(body), Some(&key.key)).await;
    assert_eq!(status, StatusCode::OK, "{json_body}");
    assert_eq!(json_body["text"], "mock transcription");
}

// ─── 8. POST /v1/search ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_search_missing_key() {
    let _g = lock();
    let (_db, _key) = init_test_env();
    let router = app_default();

    let body = json!({ "query": "rust async", "provider": "brave" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/search", Some(body), None).await;
    assert_error_envelope(status, &json_body, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_search_invalid_key() {
    let _g = lock();
    let (_db, _key) = init_test_env();
    let router = app_default();

    let body = json!({ "query": "rust async", "provider": "brave" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/search", Some(body), Some("sk-invalid")).await;
    assert_error_envelope(status, &json_body, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_search_missing_provider() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let body = json!({ "query": "rust async" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/search", Some(body), Some(&key.key)).await;
    assert_error_envelope(status, &json_body, StatusCode::BAD_REQUEST);
    assert_eq!(json_body["error"]["message"], "Missing required field: provider (or model)");
}

#[tokio::test]
async fn test_search_missing_query() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let body = json!({ "provider": "brave" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/search", Some(body), Some(&key.key)).await;
    assert_error_envelope(status, &json_body, StatusCode::BAD_REQUEST);
    assert_eq!(json_body["error"]["message"], "Missing required field: query");
}

#[tokio::test]
async fn test_search_unsupported_provider() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let body = json!({ "provider": "unsupported-search-provider", "query": "test" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/search", Some(body), Some(&key.key)).await;
    assert_error_envelope(status, &json_body, StatusCode::NOT_FOUND);
    assert!(json_body["error"]["message"].as_str().unwrap().contains("does not support search"));
}

// ─── 9. POST /v1/web/fetch ──────────────────────────────────────────────────

#[tokio::test]
async fn test_web_fetch_missing_key() {
    let _g = lock();
    let (_db, _key) = init_test_env();
    let router = app_default();

    let body = json!({ "url": "https://example.com", "provider": "firecrawl" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/web/fetch", Some(body), None).await;
    assert_error_envelope(status, &json_body, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_web_fetch_invalid_key() {
    let _g = lock();
    let (_db, _key) = init_test_env();
    let router = app_default();

    let body = json!({ "url": "https://example.com", "provider": "firecrawl" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/web/fetch", Some(body), Some("sk-invalid")).await;
    assert_error_envelope(status, &json_body, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_web_fetch_missing_provider() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let body = json!({ "url": "https://example.com" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/web/fetch", Some(body), Some(&key.key)).await;
    assert_error_envelope(status, &json_body, StatusCode::BAD_REQUEST);
    assert_eq!(json_body["error"]["message"], "Missing required field: provider (or model)");
}

#[tokio::test]
async fn test_web_fetch_missing_url() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let body = json!({ "provider": "firecrawl" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/web/fetch", Some(body), Some(&key.key)).await;
    assert_error_envelope(status, &json_body, StatusCode::BAD_REQUEST);
    assert_eq!(json_body["error"]["message"], "Missing required field: url");
}

#[tokio::test]
async fn test_web_fetch_ssrf_blocked_url() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let body = json!({ "provider": "firecrawl", "url": "http://127.0.0.1:8080/internal" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/web/fetch", Some(body), Some(&key.key)).await;
    assert_error_envelope(status, &json_body, StatusCode::BAD_REQUEST);
    assert!(json_body["error"]["message"].as_str().unwrap().contains("private or internal"));
}

#[tokio::test]
async fn test_web_fetch_unsupported_provider() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let body = json!({ "provider": "nonexistent-fetch-provider", "url": "https://example.com" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/web/fetch", Some(body), Some(&key.key)).await;
    assert_error_envelope(status, &json_body, StatusCode::NOT_FOUND);
    assert!(json_body["error"]["message"].as_str().unwrap().contains("does not support web fetch"));
}

// ─── 10. POST /v1/systemone ─────────────────────────────────────────────────

#[tokio::test]
async fn test_systemone_missing_key() {
    let _g = lock();
    let (_db, _key) = init_test_env();
    let router = app_default();

    let body = json!({ "model": "systemone", "input": "evaluate" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/systemone", Some(body), None).await;
    assert_error_envelope(status, &json_body, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_systemone_invalid_key() {
    let _g = lock();
    let (_db, _key) = init_test_env();
    let router = app_default();

    let body = json!({ "model": "systemone", "input": "evaluate" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/systemone", Some(body), Some("sk-invalid")).await;
    assert_error_envelope(status, &json_body, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_systemone_missing_model() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let body = json!({ "input": "evaluate" });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/systemone", Some(body), Some(&key.key)).await;
    assert_error_envelope(status, &json_body, StatusCode::BAD_REQUEST);
    assert_eq!(json_body["error"]["message"], "Missing model");
}

#[tokio::test]
async fn test_systemone_missing_state() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let body = json!({ "model": "systemone", "questions": {} });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/systemone", Some(body), Some(&key.key)).await;
    assert_error_envelope(status, &json_body, StatusCode::BAD_REQUEST);
    assert_eq!(json_body["error"]["message"], "Missing required field: state");
}

#[tokio::test]
async fn test_systemone_missing_questions() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let body = json!({ "model": "systemone", "state": {} });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/systemone", Some(body), Some(&key.key)).await;
    assert_error_envelope(status, &json_body, StatusCode::BAD_REQUEST);
    assert_eq!(json_body["error"]["message"], "Missing required field: questions");
}

#[tokio::test]
async fn test_systemone_valid_request() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    let body = json!({
        "model": "systemone",
        "state": { "stage": "eval" },
        "questions": { "q1": "input" }
    });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "POST", "/v1/systemone", Some(body), Some(&key.key)).await;
    assert_eq!(status, StatusCode::OK, "{json_body}");
    assert_eq!(json_body["decision"], "pass");
    assert_eq!(json_body["model"], "systemone");
}

// ─── 11. GET /v1beta/models & /v1beta/models/{*path} ────────────────────────

#[tokio::test]
async fn test_v1beta_models_get() {
    let _g = lock();
    let (_db, _key) = init_test_env();
    let router = app_default();

    let (status, _hdrs, json_body, _bytes) = oneshot_request(router, "GET", "/v1beta/models", None, None).await;
    assert_eq!(status, StatusCode::OK, "{json_body}");
    let models = json_body["models"].as_array().expect("models array");
    assert!(!models.is_empty(), "gemini model catalog should have entries");
    for m in models {
        let name = m["name"].as_str().expect("model name");
        assert!(name.starts_with("models/"), "expected 'models/' prefix: {name}");
        assert!(m["supportedGenerationMethods"].as_array().is_some());
    }
}

#[tokio::test]
async fn test_v1beta_generate_content_missing_key() {
    let _g = lock();
    let (_db, _key) = init_test_env();
    let router = app_default();

    let body = json!({ "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }] });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(
        router,
        "POST",
        "/v1beta/models/openai/gpt-4o:generateContent",
        Some(body),
        None,
    )
    .await;
    assert_error_envelope(status, &json_body, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_v1beta_generate_content_invalid_key() {
    let _g = lock();
    let (_db, _key) = init_test_env();
    let router = app_default();

    let body = json!({ "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }] });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(
        router,
        "POST",
        "/v1beta/models/openai/gpt-4o:generateContent",
        Some(body),
        Some("sk-invalid"),
    )
    .await;
    assert_error_envelope(status, &json_body, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_v1beta_generate_content_non_streaming() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let gemini_response = json!({
        "candidates": [{
            "content": { "parts": [{ "text": "Gemini response text" }], "role": "model" },
            "finishReason": "STOP",
            "index": 0
        }]
    });
    let mock = Arc::new(MockChatEngine::json(200, &gemini_response));
    let router = app_with_chat(mock.clone());

    let body = json!({
        "contents": [{ "role": "user", "parts": [{ "text": "hello gemini" }] }],
        "systemInstruction": { "parts": [{ "text": "be concise" }] }
    });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(
        router,
        "POST",
        "/v1beta/models/openai/gpt-4o:generateContent",
        Some(body),
        Some(&key.key),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json_body}");
    assert_eq!(json_body["candidates"][0]["content"]["parts"][0]["text"], "Gemini response text");

    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    let seen = mock.last.lock().unwrap().clone().expect("engine called");
    assert_eq!(seen.model, "openai/gpt-4o");
    assert!(!seen.stream, "generateContent should not stream");
}

#[tokio::test]
async fn test_v1beta_generate_content_streaming() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let chunks = vec![
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"chunk 1\"}],\"role\":\"model\"}}]}\n\n".to_string(),
        "data: [DONE]\n\n".to_string(),
    ];
    let mock = Arc::new(MockChatEngine::sse(chunks));
    let router = app_with_chat(mock.clone());

    let body = json!({
        "contents": [{ "role": "user", "parts": [{ "text": "stream test" }] }]
    });
    let (status, headers, _json_body, bytes) = oneshot_request(
        router,
        "POST",
        "/v1beta/models/openai/gpt-4o:streamGenerateContent",
        Some(body),
        Some(&key.key),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    assert!(ct.contains("text/event-stream"), "expected text/event-stream, got: {ct}");

    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("chunk 1"), "expected stream chunk: {text}");

    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    let seen = mock.last.lock().unwrap().clone().expect("engine called");
    assert_eq!(seen.model, "openai/gpt-4o");
    assert!(seen.stream, "streamGenerateContent should stream");
}

#[tokio::test]
async fn test_v1beta_generate_content_bare_model() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let gemini_response = json!({
        "candidates": [{
            "content": { "parts": [{ "text": "bare model response" }], "role": "model" }
        }]
    });
    let mock = Arc::new(MockChatEngine::json(200, &gemini_response));
    let router = app_with_chat(mock.clone());

    let body = json!({
        "contents": [{ "role": "user", "parts": [{ "text": "hi bare" }] }]
    });
    let (status, _hdrs, json_body, _bytes) = oneshot_request(
        router,
        "POST",
        "/v1beta/models/gpt-4o:generateContent",
        Some(body),
        Some(&key.key),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json_body}");
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    let seen = mock.last.lock().unwrap().clone().expect("engine called");
    assert_eq!(seen.model, "gpt-4o");
}

#[tokio::test]
async fn test_v1beta_models_catchall_get_method_not_allowed() {
    let _g = lock();
    let (_db, key) = init_test_env();
    let router = app_default();

    // GET on a POST-only action route returns 405 Method Not Allowed
    let (status, _hdrs, _json_body, _bytes) = oneshot_request(
        router,
        "GET",
        "/v1beta/models/openai/gpt-4o:generateContent",
        None,
        Some(&key.key),
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn test_v1beta_options_cors() {
    let _g = lock();
    let (_db, _key) = init_test_env();
    let router = app_default();

    let (status, headers, _json_body, _bytes) = oneshot_request(
        router,
        "OPTIONS",
        "/v1beta/models/openai/gpt-4o:generateContent",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let cors = headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).and_then(|v| v.to_str().ok()).unwrap_or("");
    assert_eq!(cors, "*");
}
