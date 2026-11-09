//! Integration tests: chat requests through the engine against a mocked
//! upstream executor, plus combo fallback across providers.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nr_core::http::{UpstreamResponse};
use nr_core::json;
use nr_core::{ExecRequest, ExecResult, Executor, Result};
use nr_db::repos::{create_combo, create_provider_connection};
use nr_db::Db;
use tempfile::TempDir;

use super::*;

fn setup_db() -> (TempDir, Db) {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    (dir, db)
}

fn add_openai_conn(db: &Db, id: &str, key: &str) {
    let conn = json!({
        "id": id,
        "provider": "openai",
        "authType": "apikey",
        "name": id,
        "priority": 1,
        "isActive": true,
        "providerSpecificData": { "apiKey": key }
    });
    create_provider_connection(db, &conn).unwrap();
}

/// A mock executor that records calls and hands back a queued response.
struct MockExecutor {
    provider: &'static str,
    responses: Mutex<std::vec::IntoIter<Result<UpstreamResponse>>>,
    calls: Arc<AtomicUsize>,
    seen_models: Mutex<Vec<String>>,
}

#[async_trait]
impl Executor for MockExecutor {
    fn provider(&self) -> &str {
        self.provider
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen_models.lock().unwrap().push(req.model.to_string());
        let response = self
            .responses
            .lock()
            .unwrap()
            .next()
            .unwrap_or_else(|| Ok(UpstreamResponse::json(200, &json!({"choices": [{"message": {"content": "ok"}}]}))))?;
        Ok(ExecResult {
            response,
            url: format!("https://upstream.test/{provider}", provider = self.provider),
            headers: Default::default(),
            transformed_body: json!({}),
            response_format: None,
        })
    }
}

fn install_mock(provider: &'static str, responses: Vec<Result<UpstreamResponse>>, calls: Arc<AtomicUsize>, seen: Arc<Mutex<Vec<String>>>) {
    let exec = Arc::new(MockExecutor {
        provider,
        responses: Mutex::new(responses.into_iter()),
        calls,
        seen_models: seen,
    });
    set_executor_resolver(Arc::new(move |_p| exec.clone() as Arc<dyn Executor>));
}

fn chat_req(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.to_string(),
        body: json!({"model": model, "messages": [{"role": "user", "content": "hi"}], "stream": false}),
        stream: false,
        headers: Default::default(),
        endpoint: "/v1/chat/completions".into(),
        api_key: None,
    }
}

#[tokio::test]
async fn single_model_success_routes_to_executor() {
    let (_dir, db) = setup_db();
    add_openai_conn(&db, "c1", "sk-test");

    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    install_mock(
        "openai",
        vec![Ok(UpstreamResponse::json(200, &json!({"choices": [{"message": {"content": "hello"}}]})))],
        calls.clone(),
        seen.clone(),
    );

    let engine = RealChatEngine::new(db);
    let resp = engine.handle_chat(chat_req("openai/gpt-4o")).await.unwrap();
    assert!(resp.ok());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(*seen.lock().unwrap(), vec!["gpt-4o".to_string()]);
}

#[tokio::test]
async fn account_fallback_rotates_on_rate_limit() {
    let (_dir, db) = setup_db();
    add_openai_conn(&db, "c1", "sk-1");
    add_openai_conn(&db, "c2", "sk-2");

    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    install_mock(
        "openai",
        vec![
            Ok(UpstreamResponse::json(429, &json!({"error": {"message": "rate limit"}}))),
            Ok(UpstreamResponse::json(200, &json!({"choices": [{"message": {"content": "ok"}}]}))),
        ],
        calls.clone(),
        seen.clone(),
    );

    let engine = RealChatEngine::new(db);
    let resp = engine.handle_chat(chat_req("openai/gpt-4o")).await.unwrap();
    assert!(resp.ok(), "second account should succeed");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "should have tried two accounts");
}

#[tokio::test]
async fn all_accounts_rate_limited_surfaces_error() {
    let (_dir, db) = setup_db();
    add_openai_conn(&db, "c1", "sk-1");

    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    install_mock(
        "openai",
        vec![Ok(UpstreamResponse::json(429, &json!({"error": {"message": "rate limit"}})))],
        calls.clone(),
        seen.clone(),
    );

    let engine = RealChatEngine::new(db);
    let err = engine.handle_chat(chat_req("openai/gpt-4o")).await.unwrap_err();
    assert_eq!(err.status(), 429);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "single account tried once");
}

#[tokio::test]
async fn request_scoped_400_is_not_rotated() {
    let (_dir, db) = setup_db();
    add_openai_conn(&db, "c1", "sk-1");
    add_openai_conn(&db, "c2", "sk-2");

    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    install_mock(
        "openai",
        vec![Ok(UpstreamResponse::json(400, &json!({"error": {"message": "context length exceeded"}})))],
        calls.clone(),
        seen.clone(),
    );

    let engine = RealChatEngine::new(db);
    let resp = engine.handle_chat(chat_req("openai/gpt-4o")).await.unwrap();
    // Request-scoped: returned as-is, no rotation to a healthy account.
    assert_eq!(resp.status(), 400);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn no_credentials_is_not_found() {
    let (_dir, db) = setup_db();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    install_mock("openai", vec![], calls, seen);

    let engine = RealChatEngine::new(db);
    let err = engine.handle_chat(chat_req("openai/gpt-4o")).await.unwrap_err();
    assert_eq!(err.status(), 404);
}

#[tokio::test]
async fn combo_falls_back_to_next_member() {
    let (_dir, db) = setup_db();
    add_openai_conn(&db, "oc1", "sk-openai");
    // second member runs on anthropic
    let conn = json!({
        "id": "ac1",
        "provider": "anthropic",
        "authType": "apikey",
        "name": "ac1",
        "priority": 1,
        "isActive": true,
        "providerSpecificData": { "apiKey": "sk-anthropic" }
    });
    create_provider_connection(&db, &conn).unwrap();
    create_combo(&db, &json!({
        "name": "combo-auto",
        "models": ["openai/gpt-4o", "anthropic/claude-3-5-sonnet"]
    })).unwrap();

    let openai_calls = Arc::new(AtomicUsize::new(0));
    let openai_seen = Arc::new(Mutex::new(Vec::new()));
    install_mock(
        "openai",
        vec![Ok(UpstreamResponse::json(429, &json!({"error": {"message": "rate limit"}})))],
        openai_calls.clone(),
        openai_seen.clone(),
    );

    let anthropic_calls = Arc::new(AtomicUsize::new(0));
    let anthropic_seen = Arc::new(Mutex::new(Vec::new()));
    // Install executors that dispatch by provider so both mocks coexist.
    let oai = Arc::new(MockExecutor {
        provider: "openai",
        responses: Mutex::new(vec![Ok(UpstreamResponse::json(429, &json!({"error": {"message": "rate limit"}})))].into_iter()),
        calls: openai_calls.clone(),
        seen_models: openai_seen.clone(),
    });
    let ant = Arc::new(MockExecutor {
        provider: "anthropic",
        responses: Mutex::new(vec![Ok(UpstreamResponse::json(200, &json!({"choices": [{"message": {"content": "from anthropic"}}]})))].into_iter()),
        calls: anthropic_calls.clone(),
        seen_models: anthropic_seen.clone(),
    });
    let oai_c = oai.clone();
    let ant_c = ant.clone();
    set_executor_resolver(Arc::new(move |p| {
        if p == "openai" { oai_c.clone() as Arc<dyn Executor> } else { ant_c.clone() as Arc<dyn Executor> }
    }));

    let engine = RealChatEngine::new(db);
    let resp = engine.handle_chat(chat_req("combo-auto")).await.unwrap();
    assert!(resp.ok(), "combo should fall back to anthropic member");
    assert!(openai_calls.load(Ordering::SeqCst) >= 1);
    assert!(anthropic_calls.load(Ordering::SeqCst) >= 1);
    drop(oai);
    drop(ant);
}

#[tokio::test]
async fn streaming_passthrough_emits_sse() {
    let (_dir, db) = setup_db();
    add_openai_conn(&db, "c1", "sk-1");

    let sse_text = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
    let mut headers = http::HeaderMap::new();
    headers.insert(http::header::CONTENT_TYPE, http::HeaderValue::from_static("text/event-stream"));
    let upstream = UpstreamResponse::from_bytes(200, headers, sse_text.to_string());

    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    install_mock("openai", vec![Ok(upstream)], calls, seen);

    let engine = RealChatEngine::new(db);
    let mut req = chat_req("openai/gpt-4o");
    req.stream = true;
    req.body.as_object_mut().unwrap().insert("stream".into(), json!(true));

    let resp = engine.handle_chat(req).await.unwrap();
    assert_eq!(resp.content_type(), "text/event-stream");
    let out = nr_core::http::collect_sse_data(resp.body).await.unwrap();
    let joined = out.join("|");
    assert!(joined.contains("\"content\":\"hi\""), "got: {joined}");
    assert!(joined.iter().any(|d| d == "[DONE]"), "got: {joined}");
}
