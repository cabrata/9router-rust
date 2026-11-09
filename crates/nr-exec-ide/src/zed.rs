//! Port of open-sse/executors/zed.js.
//!
//! Routes requests to Zed's hosted LLM aggregator (cloud.zed.dev/completions),
//! a multi-format proxy fronting Anthropic/OpenAI/Google/xAI depending on the
//! requested model. Wire protocol: POST /completions with NDJSON/SSE response,
//! authenticated with a short-lived LLM bearer token exchanged from the
//! RSA-decrypted access_token.
//!
//! Overrides execute() entirely (does NOT use DefaultExecutor's pipeline)
//! because the Zed wire shape (thread envelope, LLM-token exchange, NDJSON
//! status frames) doesn't fit the generic transformRequest/buildUrl contract.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use indexmap::IndexMap;
use nr_core::http::UpstreamResponse;
use nr_core::{ExecRequest, ExecResult, Executor, Json, JsonMap, Result};
use nr_translator::{translate_request, init_state, TranslationOptions, registry::formats as tr_formats};
use serde_json::json;

use crate::zed_auth::{
    build_zed_user_auth_header, resolve_zed_models, ZED_HEADERS, ZED_LLM_BASE_URL,
};

/// Wire values for the `provider` field of POST /completions.
/// Must match Zed's catalog spellings exactly.
const ZED_PROVIDER_ANTHROPIC: &str = "anthropic";
const ZED_PROVIDER_OPENAI: &str = "open_ai";
const ZED_PROVIDER_GOOGLE: &str = "google";
const ZED_PROVIDER_XAI: &str = "x_ai";

/// Normalize provider value from config or infer from model name.
fn normalize_zed_provider(value: Option<&str>, model: &str) -> &'static str {
    if let Some(v) = value {
        let v_lower = v.to_lowercase();
        if v_lower == "anthropic" {
            return ZED_PROVIDER_ANTHROPIC;
        }
        if v_lower == "openai" || v_lower == "open_ai" {
            return ZED_PROVIDER_OPENAI;
        }
        if v_lower == "google" || v_lower == "gemini" {
            return ZED_PROVIDER_GOOGLE;
        }
        if v_lower == "xai" || v_lower == "x_ai" || v_lower == "x-ai" {
            return ZED_PROVIDER_XAI;
        }
    }

    let m_lower = model.to_lowercase();
    if m_lower.contains("claude") {
        return ZED_PROVIDER_ANTHROPIC;
    }
    if m_lower.contains("gemini") {
        return ZED_PROVIDER_GOOGLE;
    }
    if m_lower.contains("grok") || m_lower.contains("xai") {
        return ZED_PROVIDER_XAI;
    }
    ZED_PROVIDER_OPENAI
}

/// Build the provider-specific request from OpenAI format using translators.
fn build_provider_request(
    provider: &str,
    model: &str,
    body: &Json,
    stream: bool,
    _credentials: &nr_core::Credentials,
) -> Json {
    let options = TranslationOptions::default();
    let body_copy = body.clone();

    match provider {
        ZED_PROVIDER_ANTHROPIC => {
            // Translate OpenAI format to Claude format
            translate_request(
                tr_formats::OPENAI,
                tr_formats::CLAUDE,
                model,
                body_copy,
                stream,
                None,
                &options,
            )
        }
        ZED_PROVIDER_GOOGLE => {
            // Translate OpenAI format to Gemini format
            let mut translated = translate_request(
                tr_formats::OPENAI,
                tr_formats::GEMINI,
                model,
                body_copy,
                stream,
                None,
                &options,
            );

            // Drop safetySettings for Zed Google path (Vertex vocabulary)
            if let Json::Object(ref mut obj) = translated {
                obj.remove("safetySettings");
            }
            translated
        }
        ZED_PROVIDER_OPENAI => {
            // Translate OpenAI format to OpenAI Responses format
            translate_request(
                tr_formats::OPENAI,
                tr_formats::OPENAI_RESPONSES,
                model,
                body_copy,
                stream,
                None,
                &options,
            )
        }
        // xAI is OpenAI-shaped — forward as-is
        _ => body_copy,
    }
}

/// Create an error chunk in OpenAI format.
fn create_error_chunk(model: &str, message: &str) -> Json {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    json!({
        "id": format!("chatcmpl-zed-error-{}", now * 1000),
        "object": "chat.completion.chunk",
        "created": now,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": { "content": format!("[Zed error] {}", message) },
            "finish_reason": "stop"
        }]
    })
}

/// One decoded line of Zed's NDJSON/SSE stream.
#[derive(Debug, Clone, PartialEq)]
enum ZedPayload {
    Done,
    /// The provider-shaped chunk (`payload.event`).
    Event(Json),
    /// A status frame (`payload.status`), already normalized.
    Status(Json),
}

fn unwrap_zed_line(line: &str) -> Option<ZedPayload> {
    let mut text = line.trim_end_matches('\r').trim();
    if text.is_empty() {
        return None;
    }
    if let Some(rest) = text.strip_prefix("data:") {
        text = rest.trim_start();
    }
    if text == "[DONE]" {
        return Some(ZedPayload::Done);
    }
    let Ok(parsed) = serde_json::from_str::<Json>(text) else { return None };
    match &parsed {
        Json::Object(obj) => {
            if let Some(event) = obj.get("event") {
                return Some(ZedPayload::Event(event.clone()));
            }
            if let Some(status) = obj.get("status") {
                return Some(ZedPayload::Status(normalize_status(status)?));
            }
            // No envelope keys: the line itself is the chunk.
            Some(ZedPayload::Event(parsed))
        }
        // `JSON.parse("123")` is not an object → treated as no event upstream.
        _ => Some(ZedPayload::Event(parsed)),
    }
}

/// `normalizeStatus(status)` — `{"failed": {...}}` becomes `{type: "failed", ...}`.
fn normalize_status(status: &Json) -> Option<Json> {
    match status {
        Json::String(s) => Some(json!({ "type": s })),
        Json::Object(obj) => {
            if let Some((key, value)) = obj.iter().next() {
                if value.is_object() {
                    let mut out = JsonMap::new();
                    out.insert("type".into(), Json::String(key.clone()));
                    if let Some(inner) = value.as_object() {
                        out.extend(inner.clone());
                    }
                    return Some(Json::Object(out));
                }
            }
            Some(Json::Object(obj.clone()))
        }
        _ => None,
    }
}

/// `convertProviderEvent(provider, event, state)` — provider chunk → OpenAI.
fn convert_provider_event(provider: &str, event: Option<&Json>, state: &mut nr_translator::StreamState) -> Vec<Json> {
    match provider {
        ZED_PROVIDER_ANTHROPIC => nr_translator::response::claude_to_openai::claude_to_openai_response(event, state),
        ZED_PROVIDER_GOOGLE => nr_translator::response::gemini_to_openai::gemini_to_openai_response(event, state),
        ZED_PROVIDER_OPENAI => nr_translator::response::openai_responses::openai_responses_response(event, state),
        _ => match event {
            Some(e) => vec![e.clone()],
            None => Vec::new(),
        },
    }
}

/// `initProviderState(provider, model)`.
fn init_provider_state(provider: &str, model: &str) -> nr_translator::StreamState {
    let mut state = match provider {
        ZED_PROVIDER_ANTHROPIC => init_state(tr_formats::CLAUDE),
        ZED_PROVIDER_GOOGLE => init_state(tr_formats::GEMINI),
        ZED_PROVIDER_OPENAI => init_state(tr_formats::OPENAI_RESPONSES),
        _ => init_state(tr_formats::OPENAI),
    };
    if provider == ZED_PROVIDER_OPENAI {
        state.model = Some(model.to_string());
    }
    state
}

/// `wrapZedCompletionStream(response, provider, model)` — unwrap Zed's
/// `{event|status}` NDJSON envelopes and re-emit OpenAI SSE.
pub fn wrap_completion_stream(upstream: UpstreamResponse, provider: &str, model: &str) -> UpstreamResponse {
    let provider = provider.to_string();
    let model = model.to_string();
    let (tx, rx) = tokio::sync::mpsc::channel::<std::result::Result<bytes::Bytes, std::io::Error>>(16);

    tokio::spawn(async move {
        use futures::StreamExt;

        let mut state = init_provider_state(&provider, &model);
        let mut buffer = String::new();
        // Set when a terminal envelope (`[DONE]`, `stream_ended`, `failed`) or
        // the end of the socket has been seen.
        let mut ended = false;

        /// Flush the converter's final chunk (finish_reason/usage) then `[DONE]`.
        async fn finish(
            provider: &str,
            state: &mut nr_translator::StreamState,
            tx: &tokio::sync::mpsc::Sender<std::result::Result<bytes::Bytes, std::io::Error>>,
        ) {
            for chunk in convert_provider_event(provider, None, state) {
                if !chunk.is_null() {
                    let _ = tx.send(Ok(bytes::Bytes::from(format!("data: {chunk}\n\n")))).await;
                }
            }
            let _ = tx.send(Ok(bytes::Bytes::from("data: [DONE]\n\n"))).await;
        }

        // Returns the SSE text to emit; `terminal` reports an already-finished
        // stream (the caller stops reading upstream).
        let process_line = |line: &str, state: &mut nr_translator::StreamState| -> (String, bool) {
            let mut out = String::new();
            let Some(payload) = unwrap_zed_line(line) else { return (out, false) };
            match payload {
                ZedPayload::Done => (out, true),
                ZedPayload::Status(status) => {
                    let status_type = status.get("type").and_then(Json::as_str).unwrap_or("");
                    let failed = status.get("failed");
                    if status_type == "failed" || failed.is_some() {
                        let detail = failed.unwrap_or(&status);
                        let message = detail
                            .get("message")
                            .and_then(Json::as_str)
                            .or_else(|| detail.get("error").and_then(Json::as_str))
                            .or_else(|| detail.get("code").and_then(Json::as_str))
                            .unwrap_or("request failed");
                        let chunk = create_error_chunk(&model, message);
                        out.push_str(&format!("data: {chunk}\n\n"));
                        (out, true)
                    } else if status_type == "stream_ended" {
                        (out, true)
                    } else {
                        (out, false)
                    }
                }
                ZedPayload::Event(event) => {
                    for chunk in convert_provider_event(&provider, Some(&event), state) {
                        if chunk.is_null() {
                            continue;
                        }
                        out.push_str(&format!("data: {chunk}\n\n"));
                    }
                    (out, false)
                }
            }
        };

        let mut stream = upstream.body;
        while !ended {
            match stream.next().await {
                Some(Ok(bytes)) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes));
                    let mut out = String::new();
                    while let Some(nl) = buffer.find('\n') {
                        let line: String = buffer[..nl].to_string();
                        buffer.drain(..=nl);
                        let (text, terminal) = process_line(&line, &mut state);
                        out.push_str(&text);
                        if terminal {
                            // A terminal frame ends the read loop; the upstream
                            // socket may stay open (provider keepalive).
                            ended = true;
                            break;
                        }
                    }
                    if !out.is_empty() && tx.send(Ok(bytes::Bytes::from(out))).await.is_err() {
                        return;
                    }
                }
                Some(Err(_)) | None => {
                    if !buffer.trim().is_empty() {
                        let tail = std::mem::take(&mut buffer);
                        let (text, _) = process_line(&tail, &mut state);
                        if !text.is_empty() {
                            let _ = tx.send(Ok(bytes::Bytes::from(text))).await;
                        }
                    }
                    break;
                }
            }
        }

        finish(&provider, &mut state, &tx).await;
    });

    let mut headers = http::HeaderMap::new();
    headers.insert(http::header::CONTENT_TYPE, http::HeaderValue::from_static("text/event-stream"));
    headers.insert(http::header::CACHE_CONTROL, http::HeaderValue::from_static("no-cache"));
    UpstreamResponse::from_stream(200, headers, Box::pin(futures::stream::poll_fn({
        let mut rx = rx;
        move |cx| rx.poll_recv(cx)
    })))
}

/// ZedExecutor routes requests through Zed's aggregator with format translation.
pub struct ZedExecutor {
    provider: String,
    config: &'static Json,
}

impl ZedExecutor {
    pub fn new(provider: &str) -> Self {
        let config = nr_data::provider(provider)
            .or_else(|| nr_data::provider("zed"))
            .unwrap_or_else(|| {
                // Fallback to empty config
                Box::leak(Box::new(Json::Object(JsonMap::new())))
            });
        Self {
            provider: provider.to_string(),
            config,
        }
    }

    /// Fetch the Zed model catalog and resolve the provider for the requested model.
    async fn resolve_model(&self, model: &str, credentials: &nr_core::Credentials) -> Result<&'static str> {
        let base_url = self
            .config
            .get("llmBaseUrl")
            .and_then(|v| v.as_str())
            .unwrap_or(ZED_LLM_BASE_URL);

        match resolve_zed_models(credentials, base_url, false, None).await {
            Ok(Some(catalog)) => {
                if let Some(raw_models) = catalog.get("rawModels").and_then(|v| v.as_array()) {
                    for raw in raw_models {
                        if let Some(raw_id) = raw.get("id").and_then(|v| v.as_str()) {
                            if raw_id == model {
                                let provider = raw
                                    .get("provider")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                return Ok(normalize_zed_provider(Some(provider), model));
                            }
                        }
                    }
                }
                // Not found in cache, try refresh
                match resolve_zed_models(credentials, base_url, true, None).await {
                    Ok(Some(catalog)) => {
                        if let Some(raw_models) = catalog.get("rawModels").and_then(|v| v.as_array()) {
                            for raw in raw_models {
                                if let Some(raw_id) = raw.get("id").and_then(|v| v.as_str()) {
                                    if raw_id == model {
                                        let provider = raw
                                            .get("provider")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("");
                                        return Ok(normalize_zed_provider(Some(provider), model));
                                    }
                                }
                            }
                        }
                        Ok(normalize_zed_provider(None, model))
                    }
                    _ => Ok(normalize_zed_provider(None, model)),
                }
            }
            Err(_e) => {
                // Model catalog unavailable, infer provider from model name
                Ok(normalize_zed_provider(None, model))
            }
            Ok(None) => Ok(normalize_zed_provider(None, model)),
        }
    }
}

impl nr_exec_base::base::BaseHooks for ZedExecutor {
    fn provider(&self) -> &str {
        "zed"
    }

    fn config(&self) -> &nr_core::Json {
        self.config
    }
}

#[async_trait]
impl Executor for ZedExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn no_auth(&self) -> bool {
        false
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        let provider = self.resolve_model(&req.model, &req.credentials).await?;

        let provider_request = build_provider_request(provider, &req.model, &req.body, req.stream, &req.credentials);

        let body_record = req.body.as_object().cloned().unwrap_or_default();
        let thread_id = body_record
            .get("thread_id")
            .and_then(|v| v.as_str())
            .or_else(|| req.credentials.str("_clientSessionId"))
            .map(|s| s.to_string());

        let prompt_id = body_record
            .get("prompt_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let payload = json!({
            "thread_id": thread_id,
            "prompt_id": prompt_id,
            "provider": provider,
            "model": &req.model,
            "provider_request": provider_request,
        });

        let base_url = self
            .config
            .get("llmBaseUrl")
            .and_then(|v| v.as_str())
            .unwrap_or(ZED_LLM_BASE_URL);

        let client = reqwest::Client::new();
        let auth_header = build_zed_user_auth_header(&req.credentials)?;
        let url = format!("{}/completions", base_url.trim_end_matches('/'));

        let app_version = self
            .config
            .get("appVersion")
            .and_then(|v| v.as_str())
            .unwrap_or("0.200.0");

        let req_builder = client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/x-ndjson, text/event-stream, */*")
            .header("User-Agent", "9router/zed")
            .header("x-zed-version", app_version)
            .header(ZED_HEADERS.client_supports_status, "true")
            .header(ZED_HEADERS.client_supports_stream_ended, "true")
            .header("Authorization", auth_header);

        let response = req_builder
            .json(&payload)
            .send()
            .await
            .map_err(|e| nr_core::Error::other(format!("Zed request failed: {}", e)))?;

        let upstream = UpstreamResponse::from_reqwest(response);
        // `{event|status}` NDJSON envelopes are unwrapped here and each
        // provider-shaped chunk is translated to OpenAI SSE, so downstream sees
        // a normal OpenAI stream regardless of which backend Zed fronts.
        let wrapped = if upstream.ok() {
            wrap_completion_stream(upstream, provider, &req.model)
        } else {
            upstream
        };

        let mut headers = IndexMap::new();
        headers.insert("Content-Type".to_string(), "application/json".to_string());

        Ok(ExecResult {
            response: wrapped,
            url: url.clone(),
            headers,
            transformed_body: payload,
            response_format: None,
        })
    }
}

pub fn create(provider: &str) -> Arc<dyn Executor> {
    Arc::new(ZedExecutor::new(provider))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_zed_provider_anthropic() {
        assert_eq!(normalize_zed_provider(Some("anthropic"), "any-model"), ZED_PROVIDER_ANTHROPIC);
        // Provider value is not "anthropic", so fall back to model inference
        assert_eq!(normalize_zed_provider(Some("other"), "claude-3-opus"), ZED_PROVIDER_ANTHROPIC);
        assert_eq!(normalize_zed_provider(None, "claude-3-opus"), ZED_PROVIDER_ANTHROPIC);
    }

    #[test]
    fn normalize_zed_provider_google() {
        assert_eq!(normalize_zed_provider(Some("google"), "any-model"), ZED_PROVIDER_GOOGLE);
        assert_eq!(normalize_zed_provider(Some("gemini"), "any-model"), ZED_PROVIDER_GOOGLE);
        assert_eq!(normalize_zed_provider(None, "gemini-2.0-flash"), ZED_PROVIDER_GOOGLE);
    }

    #[test]
    fn normalize_zed_provider_openai() {
        assert_eq!(normalize_zed_provider(Some("open_ai"), "any-model"), ZED_PROVIDER_OPENAI);
        assert_eq!(normalize_zed_provider(Some("openai"), "any-model"), ZED_PROVIDER_OPENAI);
        assert_eq!(normalize_zed_provider(None, "gpt-4"), ZED_PROVIDER_OPENAI);
    }

    #[test]
    fn normalize_zed_provider_xai() {
        assert_eq!(normalize_zed_provider(Some("xai"), "any-model"), ZED_PROVIDER_XAI);
        assert_eq!(normalize_zed_provider(Some("x_ai"), "any-model"), ZED_PROVIDER_XAI);
        assert_eq!(normalize_zed_provider(None, "grok-2"), ZED_PROVIDER_XAI);
    }

    #[test]
    fn unwrap_zed_line_done() {
        let payload = unwrap_zed_line("[DONE]");
        assert!(matches!(payload, Some(ZedPayload::Done)));
    }

    #[test]
    fn unwrap_zed_line_with_data_prefix() {
        let line = r#"data: {"event": "test"}"#;
        let payload = unwrap_zed_line(line);
        assert!(matches!(payload, Some(ZedPayload::Event(_))));
    }

    #[test]
    fn unwrap_extracts_event_body_not_the_envelope() {
        // The chunk is `payload.event`, never the envelope itself.
        let payload = unwrap_zed_line(r#"{"event": {"choices": [{"delta": {"content": "hi"}}]}}"#);
        match payload {
            Some(ZedPayload::Event(event)) => {
                assert_eq!(event["choices"][0]["delta"]["content"], json!("hi"));
                assert!(event.get("event").is_none());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn unwrap_normalizes_status_objects() {
        match unwrap_zed_line(r#"{"status": "stream_ended"}"#) {
            Some(ZedPayload::Status(s)) => assert_eq!(s["type"], json!("stream_ended")),
            other => panic!("unexpected {other:?}"),
        }
        match unwrap_zed_line(r#"{"status": {"failed": {"message": "boom", "code": "x"}}}"#) {
            Some(ZedPayload::Status(s)) => {
                assert_eq!(s["type"], json!("failed"));
                assert_eq!(s["message"], json!("boom"));
                assert_eq!(s["code"], json!("x"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn unwrap_treats_bare_objects_as_the_chunk() {
        match unwrap_zed_line(r#"{"choices": [{"delta": {"content": "x"}}]}"#) {
            Some(ZedPayload::Event(event)) => assert_eq!(event["choices"][0]["delta"]["content"], json!("x")),
            other => panic!("unexpected {other:?}"),
        }
        assert!(unwrap_zed_line("not json").is_none());
        assert!(unwrap_zed_line("   ").is_none());
    }

    #[test]
    fn stream_wrapper_translates_xai_openai_chunks() {
        // xAI is OpenAI-shaped: chunks pass through and [DONE] is appended once.
        let raw = concat!(
            r#"{"event": {"id":"1","object":"chat.completion.chunk","created":1,"model":"grok-2","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}}"#,
            "\n",
            r#"{"status": "stream_ended"}"#,
            "\n",
            r#"{"event": {"choices":[{"index":0,"delta":{"content":"ignored"},"finish_reason":null}]}}"#,
            "\n",
        );
        let upstream = UpstreamResponse::from_bytes(200, http::HeaderMap::new(), raw);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let text = rt.block_on(async { wrap_completion_stream(upstream, ZED_PROVIDER_XAI, "grok-2").text().await.unwrap() });
        assert!(text.contains(r#""content":"hi""#), "{text}");
        assert!(!text.contains("ignored"), "stream must stop at stream_ended: {text}");
        assert_eq!(text.matches("data: [DONE]").count(), 1);
        assert!(text.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn stream_wrapper_reports_failed_status_as_error_chunk() {
        let raw = "{\"status\":{\"failed\":{\"message\":\"trial blocked\"}}}\n";
        let upstream = UpstreamResponse::from_bytes(200, http::HeaderMap::new(), raw);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let text = rt.block_on(async { wrap_completion_stream(upstream, ZED_PROVIDER_XAI, "grok-2").text().await.unwrap() });
        assert!(text.contains("[Zed error] trial blocked"), "{text}");
        assert!(text.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn stream_wrapper_translates_claude_chunks() {
        let raw = concat!(
            r#"{"event": {"type":"message_start","message":{"id":"m1","model":"claude-3-opus","usage":{"input_tokens":5}}}}"#,
            "\n",
            r#"{"event": {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hey"}}}"#,
            "\n",
            r#"{"event": {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}}"#,
            "\n",
            r#"{"event": {"type":"message_stop"}}"#,
            "\n",
        );
        let upstream = UpstreamResponse::from_bytes(200, http::HeaderMap::new(), raw);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let text = rt.block_on(async { wrap_completion_stream(upstream, ZED_PROVIDER_ANTHROPIC, "claude-3-opus").text().await.unwrap() });
        assert!(text.contains("hey"), "claude delta must be translated: {text}");
        assert!(!text.contains("content_block_delta"), "provider events must not leak: {text}");
        assert!(text.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn stream_wrapper_translates_gemini_chunks() {
        let raw = concat!(
            r#"{"event": {"response": {"responseId":"r1","modelVersion":"gemini-2.0-flash","candidates":[{"content":{"parts":[{"text":"yo"}]}}]}}}"#,
            "\n",
            r#"{"event": "[DONE]"}"#,
            "\n",
        );
        let upstream = UpstreamResponse::from_bytes(200, http::HeaderMap::new(), raw);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let text = rt.block_on(async { wrap_completion_stream(upstream, ZED_PROVIDER_GOOGLE, "gemini-2.0-flash").text().await.unwrap() });
        assert!(text.contains("yo"), "gemini text must be translated: {text}");
        assert!(text.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn zed_executor_creates() {
        let executor = ZedExecutor::new("zed");
        assert_eq!(executor.provider(), "zed");
    }
}
