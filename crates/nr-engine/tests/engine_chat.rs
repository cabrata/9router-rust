//! Engine-level integration tests: a chat request through [`RealChatEngine`]
//! against a mocked executor, combo fallback across providers, account
//! rotation, and the streaming chunk shape the client receives.
//!
//! Kept in its own test binary because the engine's executor resolver is a
//! process-global static; every test here serialises on [`ENGINE_LOCK`].

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nr_core::http::UpstreamResponse;
use nr_core::{json, ExecRequest, ExecResult, Executor, Json, Result};
use nr_db::repos::{create_combo, create_provider_connection};
use nr_db::Db;
use nr_engine::{ChatEngine, ChatRequest, RealChatEngine};
use serde_json::Value;
use tempfile::TempDir;

/// Serialises tests that install a process-global executor resolver.
static ENGINE_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    ENGINE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn setup_db() -> (TempDir, Db) {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    (dir, db)
}

fn add_conn(db: &Db, id: &str, provider: &str, key: &str) {
    create_provider_connection(
        db,
        &json!({
            "id": id,
            "provider": provider,
            "authType": "apikey",
            "name": id,
            "priority": 1,
            "isActive": true,
            "apiKey": key
        }),
    )
    .unwrap();
}

/// Replays a queued script of responses; records every call it receives.
struct ScriptedExecutor {
    provider: String,
    script: Mutex<std::vec::IntoIter<Result<(u16, Value, Option<String>)>>>,
    calls: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<Value>>>,
}

#[async_trait]
impl Executor for ScriptedExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.bodies.lock().unwrap().push(req.body.clone());
        let (status, body, sse) = self
            .script
            .lock()
            .unwrap()
            .next()
            .unwrap_or_else(|| Ok((200, json!({"choices": []}), None)))?;
        let response = match sse {
            Some(text) => {
                let mut h = http::HeaderMap::new();
                h.insert(http::header::CONTENT_TYPE, http::HeaderValue::from_static("text/event-stream"));
                UpstreamResponse::from_bytes(status, h, text)
            }
            None => UpstreamResponse::json(status, &body),
        };
        Ok(ExecResult {
            response,
            url: format!("https://{}.test/v1", self.provider),
            headers: Default::default(),
            transformed_body: req.body.clone(),
            response_format: None,
        })
    }
}

/// Route by provider, falling back to the last installed executor.
fn install(script: Vec<Result<(u16, Value, Option<String>)>>) -> (Arc<AtomicUsize>, Arc<Mutex<Vec<Value>>>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let exec = Arc::new(ScriptedExecutor {
        provider: "openai".into(),
        script: Mutex::new(script.into_iter()),
        calls: calls.clone(),
        bodies: bodies.clone(),
    });
    nr_engine::chat::set_executor_resolver(Arc::new(move |_p| exec.clone() as Arc<dyn Executor>));
    (calls, bodies)
}

fn chat_req(model: &str, stream: bool) -> ChatRequest {
    ChatRequest {
        model: model.to_string(),
        body: json!({
            "model": model,
            "messages": [{ "role": "user", "content": "hi" }],
            "stream": stream
        }),
        stream,
        headers: Default::default(),
        endpoint: "/v1/chat/completions".into(),
        api_key: None,
    }
}

const OK_CHOICE: &str = r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}]}"#;

fn ok() -> Result<(u16, Value, Option<String>)> {
    Ok((200, serde_json::from_str(OK_CHOICE).unwrap(), None))
}

fn status(code: u16, message: &str) -> Result<(u16, Value, Option<String>)> {
    Ok((code, json!({"error": {"message": message}}), None))
}

// ─── Single model ───────────────────────────────────────────────────────────

#[tokio::test]
async fn request_reaches_executor_with_translated_body() {
    let _g = lock();
    let (_d, db) = setup_db();
    add_conn(&db, "c1", "openai", "sk-1");
    let (calls, bodies) = install(vec![ok()]);

    let engine = RealChatEngine::new(db);
    let resp = engine.handle_chat(chat_req("openai/gpt-4o", false)).await.unwrap();

    assert_eq!(resp.status, 200);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let body = bodies.lock().unwrap()[0].clone();
    // The provider model is the bare id; the client prefix is stripped.
    assert_eq!(body["model"], "gpt-4o", "{body}");
    assert_eq!(body["messages"][0]["content"], "hi");
}

/// A bare model id is treated as an alias: the provider is inferred from the
/// name, so an unknown alias falls through to a credentials lookup miss.
#[tokio::test]
async fn unknown_bare_model_is_a_lookup_miss() {
    let _g = lock();
    let (_d, db) = setup_db();
    install(vec![ok()]);
    let engine = RealChatEngine::new(db);

    // No slash at all: not a provider/model pair and not a known alias.
    let err = engine.handle_chat(chat_req("totally-unknown-model", false)).await.unwrap_err();
    assert!(err.status() >= 400, "got {}", err.status());
}

#[tokio::test]
async fn missing_credentials_surface_404() {
    let _g = lock();
    let (_d, db) = setup_db();
    let (calls, _b) = install(vec![ok()]);
    let engine = RealChatEngine::new(db);

    let err = engine.handle_chat(chat_req("openai/gpt-4o", false)).await.unwrap_err();
    assert_eq!(err.status(), 404);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "nothing to send upstream");
}

#[tokio::test]
async fn second_account_serves_after_first_is_rate_limited() {
    let _g = lock();
    let (_d, db) = setup_db();
    add_conn(&db, "c1", "openai", "sk-1");
    add_conn(&db, "c2", "openai", "sk-2");
    // First attempt 429, second serves.
    let (calls, _b) = install(vec![status(429, "rate limit"), ok()]);

    let engine = RealChatEngine::new(db);
    let resp = engine.handle_chat(chat_req("openai/gpt-4o", false)).await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn request_scoped_failure_is_returned_verbatim() {
    let _g = lock();
    let (_d, db) = setup_db();
    add_conn(&db, "c1", "openai", "sk-1");
    add_conn(&db, "c2", "openai", "sk-2");
    // A context-length 400 is the request's fault: do not burn account two.
    let (calls, _b) = install(vec![status(400, "context length exceeded")]);

    let engine = RealChatEngine::new(db);
    let resp = engine.handle_chat(chat_req("openai/gpt-4o", false)).await.unwrap();
    assert_eq!(resp.status, 400);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // The client gets the OpenAI error envelope, not the raw upstream body.
    let bytes = resp.bytes().await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(body["error"]["message"].as_str().unwrap().contains("context length"));
}

// ─── Combo ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn combo_tries_members_in_order_and_returns_first_success() {
    let _g = lock();
    let (_d, db) = setup_db();
    add_conn(&db, "o1", "openai", "sk-o");
    add_conn(&db, "a1", "anthropic", "sk-a");
    create_combo(&db, &json!({"name": "my-combo", "models": ["openai/gpt-4o", "anthropic/claude-sonnet-4"]})).unwrap();

    // Route by provider so each member gets its own script.
    let oai_calls = Arc::new(AtomicUsize::new(0));
    let ant_calls = Arc::new(AtomicUsize::new(0));
    let oai = Arc::new(ScriptedExecutor {
        provider: "openai".into(),
        script: Mutex::new(vec![status(429, "rate limit")].into_iter()),
        calls: oai_calls.clone(),
        bodies: Arc::new(Mutex::new(Vec::new())),
    });
    let ant = Arc::new(ScriptedExecutor {
        provider: "anthropic".into(),
        script: Mutex::new(vec![ok()].into_iter()),
        calls: ant_calls.clone(),
        bodies: Arc::new(Mutex::new(Vec::new())),
    });
    let (o, a) = (oai.clone(), ant.clone());
    nr_engine::chat::set_executor_resolver(Arc::new(move |p| {
        if p == "anthropic" { a.clone() as Arc<dyn Executor> } else { o.clone() as Arc<dyn Executor> }
    }));

    let engine = RealChatEngine::new(db);
    let resp = engine.handle_chat(chat_req("my-combo", false)).await.unwrap();
    assert_eq!(resp.status, 200, "second member should serve");
    assert_eq!(oai_calls.load(Ordering::SeqCst), 1);
    assert_eq!(ant_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn combo_member_rotates_its_own_accounts_before_advancing() {
    let _g = lock();
    let (_d, db) = setup_db();
    // First member has two accounts: the first 429s, the second serves, so the
    // combo never needs the second member.
    add_conn(&db, "o1", "openai", "sk-o1");
    add_conn(&db, "o2", "openai", "sk-o2");
    add_conn(&db, "a1", "anthropic", "sk-a");
    create_combo(&db, &json!({"name": "acct-combo", "models": ["openai/gpt-4o", "anthropic/claude-sonnet-4"]})).unwrap();

    let oai_calls = Arc::new(AtomicUsize::new(0));
    let ant_calls = Arc::new(AtomicUsize::new(0));
    let oai = Arc::new(ScriptedExecutor {
        provider: "openai".into(),
        script: Mutex::new(vec![status(429, "rate limit"), ok()].into_iter()),
        calls: oai_calls.clone(),
        bodies: Arc::new(Mutex::new(Vec::new())),
    });
    let ant = Arc::new(ScriptedExecutor {
        provider: "anthropic".into(),
        script: Mutex::new(vec![ok()].into_iter()),
        calls: ant_calls.clone(),
        bodies: Arc::new(Mutex::new(Vec::new())),
    });
    let (o, a) = (oai.clone(), ant.clone());
    nr_engine::chat::set_executor_resolver(Arc::new(move |p| {
        if p == "anthropic" { a.clone() as Arc<dyn Executor> } else { o.clone() as Arc<dyn Executor> }
    }));

    let engine = RealChatEngine::new(db);
    let resp = engine.handle_chat(chat_req("acct-combo", false)).await.unwrap();
    assert_eq!(resp.status, 200, "second account of the first member serves");
    assert_eq!(oai_calls.load(Ordering::SeqCst), 2, "both openai accounts tried");
    assert_eq!(ant_calls.load(Ordering::SeqCst), 0, "combo member 2 never used");
}

#[tokio::test]
async fn combo_exhaustion_surfaces_last_status() {
    let _g = lock();
    let (_d, db) = setup_db();
    add_conn(&db, "o1", "openai", "sk-o");
    add_conn(&db, "a1", "anthropic", "sk-a");
    create_combo(&db, &json!({"name": "dead-combo", "models": ["openai/gpt-4o", "anthropic/claude-sonnet-4"]})).unwrap();

    let oai = Arc::new(ScriptedExecutor {
        provider: "openai".into(),
        script: Mutex::new(vec![status(429, "rate limit")].into_iter()),
        calls: Arc::new(AtomicUsize::new(0)),
        bodies: Arc::new(Mutex::new(Vec::new())),
    });
    let ant = Arc::new(ScriptedExecutor {
        provider: "anthropic".into(),
        script: Mutex::new(vec![status(503, "overloaded")].into_iter()),
        calls: Arc::new(AtomicUsize::new(0)),
        bodies: Arc::new(Mutex::new(Vec::new())),
    });
    let (o, a) = (oai.clone(), ant.clone());
    nr_engine::chat::set_executor_resolver(Arc::new(move |p| {
        if p == "anthropic" { a.clone() as Arc<dyn Executor> } else { o.clone() as Arc<dyn Executor> }
    }));

    let engine = RealChatEngine::new(db);
    let resp = engine.handle_chat(chat_req("dead-combo", false)).await.unwrap();
    // The last member's real upstream error is what the client sees.
    assert_eq!(resp.status, 503);
    let body: Value = serde_json::from_slice(&resp.bytes().await.unwrap()).unwrap();
    assert!(body["error"]["message"].as_str().unwrap().contains("overloaded"), "{body}");
}

#[tokio::test]
async fn combo_with_no_members_is_an_error() {
    let _g = lock();
    let (_d, db) = setup_db();
    install(vec![ok()]);
    create_combo(&db, &json!({"name": "empty-combo", "models": []})).unwrap();

    let engine = RealChatEngine::new(db);
    let err = engine.handle_chat(chat_req("empty-combo", false)).await.unwrap_err();
    assert!(err.status() >= 400, "got status {}", err.status());
}

// ─── Streaming ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn streaming_passthrough_frame_shape() {
    let _g = lock();
    let (_d, db) = setup_db();
    add_conn(&db, "c1", "openai", "sk-1");

    let upstream = concat!(
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n",
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"he\"}}]}\n\n",
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"llo\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let (_calls, _b) = install(vec![Ok((200, json!({}), Some(upstream.to_string())))]);

    let engine = RealChatEngine::new(db);
    let resp = engine.handle_chat(chat_req("openai/gpt-4o", true)).await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.content_type(), "text/event-stream");

    let data = nr_core::http::collect_sse_data(resp.body).await.unwrap();
    assert_eq!(data.last().map(String::as_str), Some("[DONE]"), "stream ends with the sentinel");

    let chunks: Vec<Value> = data
        .iter()
        .filter(|d| *d != "[DONE]")
        .map(|d| serde_json::from_str(d).unwrap_or_else(|e| panic!("bad frame {d}: {e}")))
        .collect();
    assert!(!chunks.is_empty());
    for c in &chunks {
        assert_eq!(c["object"], "chat.completion.chunk");
        assert!(c["choices"].as_array().is_some_and(|a| !a.is_empty()));
    }
    let text: String = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(text, "hello", "deltas concatenate to the full message");
    // The final content chunk carries the finish reason.
    assert_eq!(chunks.last().unwrap()["choices"][0]["finish_reason"], "stop");
}

#[tokio::test]
async fn streaming_request_scoped_error_emits_sse_error_frame() {
    let _g = lock();
    let (_d, db) = setup_db();
    add_conn(&db, "c1", "openai", "sk-1");
    add_conn(&db, "c2", "openai", "sk-2");
    install(vec![status(400, "context length exceeded")]);

    let engine = RealChatEngine::new(db);
    let resp = engine.handle_chat(chat_req("openai/gpt-4o", true)).await.unwrap();
    assert_eq!(resp.status, 400);
    assert_eq!(resp.content_type(), "application/json", "error frames stay JSON-typed");

    let data = nr_core::http::collect_sse_data(resp.body).await.unwrap();
    let joined = data.join("|");
    assert!(joined.contains("\"error\""), "{joined}");
    assert!(joined.contains("context length"), "{joined}");
}

// ─── Request translation into the executor ─────────────────────────────────

#[tokio::test]
async fn claude_endpoint_translates_to_the_provider_format() {
    let _g = lock();
    let (_d, db) = setup_db();
    add_conn(&db, "a1", "anthropic", "sk-a");
    let (calls, bodies) = install(vec![ok()]);

    let engine = RealChatEngine::new(db);
    let mut req = chat_req("anthropic/claude-sonnet-4", false);
    req.endpoint = "/v1/messages".into();
    req.body = json!({
        "model": "claude-sonnet-4",
        "max_tokens": 128,
        "messages": [{ "role": "user", "content": "hi" }]
    });
    req.body["stream"] = json!(false);

    let resp = engine.handle_chat(req).await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let sent = bodies.lock().unwrap()[0].clone();
    // Native Anthropic target: the client messages survive translation into the
    // provider body (a bare provider/model keeps its Claude wire format).
    let text = sent["messages"][0]["content"].as_str().map(str::to_string)
        .or_else(|| sent["messages"][0]["content"].as_array().and_then(|parts| parts.iter().find_map(|p| p.get("text").and_then(Json::as_str).map(str::to_string))));
    assert_eq!(text.as_deref(), Some("hi"), "user message survives: {sent}");
}

#[tokio::test]
async fn executor_network_failure_rotates_then_reports() {
    let _g = lock();
    let (_d, db) = setup_db();
    add_conn(&db, "c1", "openai", "sk-1");

    let exec = Arc::new(ScriptedExecutor {
        provider: "openai".into(),
        // A transport error (not an upstream status) is Err, not Ok(status).
        script: Mutex::new(vec![Err(nr_core::Error::Network("connection refused".into()))].into_iter()),
        calls: Arc::new(AtomicUsize::new(0)),
        bodies: Arc::new(Mutex::new(Vec::new())),
    });
    nr_engine::chat::set_executor_resolver(Arc::new(move |_p| exec.clone() as Arc<dyn Executor>));

    let engine = RealChatEngine::new(db);
    let err = engine.handle_chat(chat_req("openai/gpt-4o", false)).await.unwrap_err();
    assert!(err.status() >= 500, "transport failure maps to 5xx, got {}", err.status());
}
