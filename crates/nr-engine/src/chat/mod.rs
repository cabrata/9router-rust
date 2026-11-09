//! The real chat engine: provider detection, request translation, account
//! rotation, executor dispatch and response translation. Port of
//! src/sse/handlers/chat.js (`handleChat`, `handleSingleModelChat`) and
//! open-sse/handlers/chatCore.js (`handleChatCore`).

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use nr_core::http::{ProxyOptions, UpstreamResponse};
use nr_core::json;
use nr_core::{Error, Json, Result};
use nr_db::repos::{get_combo_by_name, get_settings};
use nr_db::Db;
use nr_executors::get_executor;
use tokio_util::sync::CancellationToken;

use crate::combo::{handle_combo_chat, ComboOutcome};
use crate::credentials::{
    clear_account_error, get_provider_credentials, mark_account_unavailable, persist_refreshed_credentials,
    CredentialLookup, SelectedCredential,
};
use crate::model::get_model_info;
use crate::stream::{build_stream_error_bytes, json_error, transform_sse_to_client};
use crate::translate::{detect_format_by_endpoint, target_format_for, translate_request_to_provider};

/// A chat request handed over from the HTTP gateway.
#[derive(Clone, Debug)]
pub struct ChatRequest {
    pub model: String,
    pub body: Json,
    pub stream: bool,
    pub headers: nr_core::http::HeaderList,
    pub endpoint: String,
    pub api_key: Option<String>,
}

/// Pluggable chat engine. The gateway dispatches through [`handle_chat`],
/// which delegates to whichever engine is installed via [`set_chat_engine`].
#[async_trait]
pub trait ChatEngine: Send + Sync {
    async fn handle_chat(&self, req: ChatRequest) -> Result<UpstreamResponse>;
}

// ─── Executor resolution hook (lets tests inject a mock executor) ───────────
type ResolveExecutor = Arc<dyn Fn(&str) -> Arc<dyn nr_core::Executor> + Send + Sync>;
static EXECUTOR_RESOLVER: parking_lot::RwLock<Option<ResolveExecutor>> = parking_lot::RwLock::new(None);

/// Resolve the executor for a provider (JS `getExecutor`).
pub fn resolve_executor(provider: &str) -> Arc<dyn nr_core::Executor> {
    if let Some(resolver) = EXECUTOR_RESOLVER.read().as_ref() {
        return resolver(provider);
    }
    get_executor(provider)
}

/// Override executor resolution (tests inject a mock executor).
pub fn set_executor_resolver(resolver: ResolveExecutor) {
    *EXECUTOR_RESOLVER.write() = Some(resolver);
}

pub fn clear_executor_resolver() {
    *EXECUTOR_RESOLVER.write() = None;
}

/// The real engine: talks to nr-db, nr-translator and nr-executors.
pub struct RealChatEngine {
    pub db: Db,
}

impl RealChatEngine {
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

#[async_trait]
impl ChatEngine for RealChatEngine {
    async fn handle_chat(&self, req: ChatRequest) -> Result<UpstreamResponse> {
        let settings = get_settings(&self.db).unwrap_or_else(|_| json!({}));

        // Claude Code marks a 1M-context request as `<model>[1m]`; the marker
        // matches no combo/alias/provider pair, so strip it before resolution.
        let model_str = req.model.trim().to_string();

        // Combo? Rotate across its members.
        if let Ok(Some(combo)) = get_combo_by_name(&self.db, &model_str) {
            if !combo.models.is_empty() {
                let strategy = combo_strategy(&settings, &model_str);
                let db = self.db.clone();
                let endpoint = req.endpoint.clone();
                let stream = req.stream;
                let headers = req.headers.clone();
                let body = req.body.clone();
                let settings = settings.clone();
                let try_member: crate::combo::TryMemberFn<'static> = Box::new(move |member: &str| {
                    let db = db.clone();
                    let endpoint = endpoint.clone();
                    let headers = headers.clone();
                    let body = body.clone();
                    let settings = settings.clone();
                    let member = member.to_string();
                    Box::pin(async move {
                        let engine = RealChatEngine { db };
                        let inner = ChatRequest {
                            model: member,
                            body,
                            stream,
                            headers,
                            endpoint,
                            api_key: None,
                        };
                        single_model_chat(&engine.db, inner, &settings).await
                    })
                });
                let outcome = handle_combo_chat(&combo.models, &model_str, strategy, try_member).await;
                return match outcome {
                    ComboOutcome::Response(r) => Ok(r),
                    ComboOutcome::Exhausted { status, error, response } => match response {
                        Some(r) => Ok(r),
                        None => Err(error_with_body(status, &error)),
                    },
                };
            }
        }

        single_model_chat(&self.db, req, &settings).await
    }
}

fn combo_strategy<'a>(settings: &'a Json, combo_name: &str) -> &'a str {
    settings
        .get("comboStrategies")
        .and_then(|s| s.get(combo_name))
        .and_then(|s| s.get("fallbackStrategy"))
        .and_then(Json::as_str)
        .or_else(|| settings.get("comboStrategy").and_then(Json::as_str))
        .unwrap_or("fallback")
}

/// Execute a single `provider/model` request with account rotation.
/// Port of JS `handleSingleModelChat`.
async fn single_model_chat(db: &Db, req: ChatRequest, settings: &Json) -> Result<UpstreamResponse> {
    let (provider, model) = get_model_info(db, &req.model);
    let Some(provider) = provider else {
        return Err(Error::BadRequest("Invalid model format".into()));
    };

    let source_format = detect_format_by_endpoint(&req.endpoint, &req.body);
    let strategy = crate::credentials::strategy_for_provider(settings, &provider);
    let client_tool = nr_common::client_detector::detect_client_tool(
        Some(&headers_as_map(&req.headers)),
        Some(&req.body),
    );
    let passthrough = nr_common::client_detector::is_native_passthrough(client_tool, &provider);

    let mut exclude: HashSet<String> = HashSet::new();
    let mut last_status: u16 = 503;
    let mut last_error = String::from("Unavailable");

    loop {
        let lookup = get_provider_credentials(db, &provider, &exclude, Some(&model), strategy);
        match lookup {
            CredentialLookup::None => {
                if exclude.is_empty() {
                    return Err(Error::NotFound(format!("No active credentials for provider: {provider}")));
                }
                return Err(error_with_body(last_status, &last_error));
            }
            CredentialLookup::AllRateLimited { retry_after_human, last_error: le, .. } => {
                let msg = if last_error != "Unavailable" {
                    last_error.clone()
                } else {
                    le.unwrap_or_else(|| "all accounts rate-limited".into())
                };
                let _ = retry_after_human;
                return Err(error_with_body(503, &msg));
            }
            CredentialLookup::Selected(cred) => {
                let selected: &SelectedCredential = cred.as_ref();
                let attempt = execute_on_account(
                    db,
                    req.body.clone(),
                    req.stream,
                    source_format,
                    provider.clone(),
                    model.clone(),
                    passthrough,
                    client_tool,
                    selected,
                    settings,
                )
                .await;

                match attempt {
                    Ok(response) => {
                        clear_account_error(db, &selected.connection_id, Some(&model));
                        return Ok(response);
                    }
                    Err(AttemptError::Fatal(resp)) => return Ok(resp),
                    Err(AttemptError::Rotate { status, error }) => {
                        mark_account_unavailable(
                            db,
                            &selected.connection_id,
                            status,
                            &error,
                            Some(&provider),
                            Some(&model),
                            0,
                        );
                        exclude.insert(selected.connection_id.clone());
                        last_status = status;
                        last_error = error;
                        continue;
                    }
                }
            }
        }
    }
}

/// Outcome of one account attempt.
enum AttemptError {
    /// The upstream error is the answer (request-scoped, or last account).
    Fatal(UpstreamResponse),
    /// Rotate to the next account.
    Rotate { status: u16, error: String },
}

/// Run chatCore against one account/connection.
#[allow(clippy::too_many_arguments)]
async fn execute_on_account(
    db: &Db,
    body: Json,
    stream: bool,
    source_format: &'static str,
    provider: String,
    model: String,
    passthrough: bool,
    client_tool: Option<&'static str>,
    selected: &SelectedCredential,
    settings: &Json,
) -> std::result::Result<UpstreamResponse, AttemptError> {
    let mut credentials = selected.credentials.clone();
    let target_format = target_format_for(&provider, &credentials);

    let mut translated_body = if passthrough {
        let mut b = body.clone();
        if let Some(obj) = b.as_object_mut() {
            obj.insert("model".into(), json!(model));
        }
        b
    } else {
        translate_request_to_provider(
            source_format,
            target_format,
            &model,
            body.clone(),
            stream,
            Some(&mut credentials),
        )
    };

    // RTK token saver: compress tool_result blobs on the final body
    // (chatCore applies it post-translate for non-cursor providers).
    let rtk_enabled = settings.get("rtkEnabled").and_then(Json::as_bool).unwrap_or(false);
    if rtk_enabled {
        crate::rtk::compress_messages(&mut translated_body, true);
    }

    let executor = resolve_executor(&provider);
    let cancel = CancellationToken::new();
    let log = nr_core::Logger::new();
    let proxy = ProxyOptions::from_credentials(&credentials);

    let exec_req = nr_core::ExecRequest {
        model: &model,
        // Clone: the refresh-and-retry path below re-sends the same body.
        body: translated_body.clone(),
        stream,
        credentials: &mut credentials,
        provider_session_id: None,
        client_tool: client_tool.map(str::to_string),
        cancel: cancel.clone(),
        log: &log,
        proxy,
        provider_overrides: None,
    };

    let exec = match executor.execute(exec_req).await {
        Ok(r) => r,
        Err(e) => {
            // Transport failure: rotate to the next account.
            return Err(AttemptError::Rotate {
                status: e.status(),
                error: e.to_string(),
            });
        }
    };

    let response = exec.response;

    // Auth failure: try a credential refresh once, then retry.
    if !executor.no_auth() && (response.status == 401 || response.status == 403) {
        if let Ok(Some(patch)) = executor.refresh_credentials(&credentials, &log, None).await {
            credentials.merge(&patch);
            persist_refreshed_credentials(db, &selected.connection_id, &credentials);
            // Build the proxy first: `credentials` is mutably borrowed by the
            // request literal below, so it cannot be borrowed again there.
            let retry_proxy = ProxyOptions::from_credentials(&credentials);
            let retry_body = translated_body.clone();
            let retry_req = nr_core::ExecRequest {
                model: &model,
                body: retry_body,
                stream,
                credentials: &mut credentials,
                provider_session_id: None,
                client_tool: client_tool.map(str::to_string),
                cancel: cancel.clone(),
                log: &log,
                proxy: retry_proxy,
                provider_overrides: None,
            };
            if let Ok(retry) = executor.execute(retry_req).await {
                if retry.response.ok() {
                    return Ok(finish_response(retry.response, retry.response_format.as_deref(), target_format, source_format, stream, cancel));
                }
            }
        }
    }

    if response.ok() {
        return Ok(finish_response(response, exec.response_format.as_deref(), target_format, source_format, stream, cancel));
    }

    // Error status: read the body to classify, then decide rotate vs fatal.
    let status = response.status;
    let body_text = response.text().await.unwrap_or_default();
    let error_text = extract_error_message(&body_text).unwrap_or_else(|| format!("HTTP {}", status));
    let decision = crate::account_fallback::check_fallback_error(status, &error_text, 0, Some(&provider));

    if decision.request_scoped && stream {
        // A request-scoped 4xx on a streaming request: hand back a clean SSE error
        // frame so the client sees the real cause rather than a truncated stream.
        let bytes = build_stream_error_bytes(status, &error_text, source_format);
        let headers = error_headers();
        return Err(AttemptError::Fatal(UpstreamResponse::from_bytes(status, headers, bytes)));
    }
    if decision.request_scoped {
        let payload = json_error(status, &error_text);
        return Err(AttemptError::Fatal(UpstreamResponse::json(status, &payload)));
    }

    Err(AttemptError::Rotate { status, error: error_text })
}

/// Turn a raw executor response into a client-facing response: streaming
/// requests get the SSE transform pipeline, JSON gets a passthrough.
fn finish_response(
    response: UpstreamResponse,
    response_format: Option<&str>,
    target_format: &'static str,
    source_format: &'static str,
    stream: bool,
    cancel: CancellationToken,
) -> UpstreamResponse {
    let upstream_format: &'static str = match response_format {
        Some(f) => leak_format(f),
        None => target_format,
    };
    if stream {
        let is_sse = response.content_type().contains("text/event-stream");
        if is_sse {
            let body = transform_sse_to_client(response.body, upstream_format, source_format, cancel);
            return UpstreamResponse::sse(body);
        }
    }
    response
}

fn error_headers() -> http::HeaderMap {
    let mut headers = http::HeaderMap::new();
    headers.insert(http::header::CONTENT_TYPE, http::HeaderValue::from_static("application/json"));
    headers
}

/// Header lookup for `detect_client_tool` expects lowercase keys (the JS
/// `headers` object it reads is case-insensitive). `HeaderList` preserves the
/// client's original casing, so normalize on the way in.
fn headers_as_map(headers: &nr_core::http::HeaderList) -> std::collections::HashMap<String, String> {
    headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
        .collect()
}

fn extract_error_message(body: &str) -> Option<String> {
    let v: Json = serde_json::from_str(body).ok()?;
    v.get("error")
        .and_then(|e| e.get("message").and_then(Json::as_str).map(str::to_string))
        .or_else(|| v.get("error").and_then(Json::as_str).map(str::to_string))
        .or_else(|| v.get("message").and_then(Json::as_str).map(str::to_string))
}

/// Formats are `'static` consts in the translator; the executor returns a
/// `String`, so intern the common ones and fall back to a leaked string.
fn leak_format(f: &str) -> &'static str {
    match f {
        "openai" => "openai",
        "openai-responses" => "openai-responses",
        "openai-response" => "openai-response",
        "claude" => "claude",
        "gemini" => "gemini",
        "gemini-cli" => "gemini-cli",
        "vertex" => "vertex",
        "codex" => "codex",
        "antigravity" => "antigravity",
        "kiro" => "kiro",
        "cursor" => "cursor",
        "ollama" => "ollama",
        "commandcode" => "commandcode",
        other => Box::leak(other.to_string().into_boxed_str()),
    }
}

/// Build a JSON error body as an `UpstreamResponse` with `Error`-like semantics.
fn error_with_body(status: u16, message: &str) -> Error {
    match status {
        401 => Error::Provider(message.to_string()),
        403 => Error::Provider(message.to_string()),
        404 => Error::NotFound(message.to_string()),
        429 => Error::Upstream { status: 429, message: message.to_string() },
        400 => Error::BadRequest(message.to_string()),
        _ => Error::Upstream { status, message: message.to_string() },
    }
}

// ─── Default pass-through engine (used before a real DB-backed engine is set) ──

pub struct PassThroughChatEngine;

#[async_trait]
impl ChatEngine for PassThroughChatEngine {
    async fn handle_chat(&self, req: ChatRequest) -> Result<UpstreamResponse> {
        let created = nr_core::json::now_ms() / 1000;
        let id = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());
        if req.stream {
            let text = nr_common::sse::chat_chunk_sse(
                &id,
                created,
                &req.model,
                &json!({"role": "assistant", "content": "pass-through"}),
                None,
            ) + nr_common::sse_constants::SSE_DONE;
            let body = UpstreamResponse::from_bytes(200, Default::default(), text).body;
            return Ok(UpstreamResponse::sse(body));
        }
        let resp = json!({
            "id": id,
            "object": "chat.completion",
            "created": created,
            "model": req.model,
            "choices": [{ "index": 0, "message": { "role": "assistant", "content": "pass-through" }, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 10, "total_tokens": 20 }
        });
        Ok(UpstreamResponse::json(200, &resp))
    }
}

/// Installed engine; `None` means the built-in pass-through engine. `Arc::new`
/// is not const, so the default engine is constructed at call time.
static ACTIVE_CHAT_ENGINE: parking_lot::RwLock<Option<Arc<dyn ChatEngine>>> =
    parking_lot::RwLock::new(None);

/// Execute a chat request with the currently active engine.
pub async fn handle_chat(req: ChatRequest) -> Result<UpstreamResponse> {
    let engine = ACTIVE_CHAT_ENGINE.read().as_ref().map(Arc::clone);
    match engine {
        Some(engine) => engine.handle_chat(req).await,
        None => PassThroughChatEngine.handle_chat(req).await,
    }
}

/// Install a chat engine (e.g. the DB-backed [`RealChatEngine`], or a mock in tests).
pub fn set_chat_engine(engine: Arc<dyn ChatEngine>) {
    *ACTIVE_CHAT_ENGINE.write() = Some(engine);
}

/// Reset to the default pass-through chat engine.
pub fn reset_chat_engine() {
    *ACTIVE_CHAT_ENGINE.write() = None;
}
