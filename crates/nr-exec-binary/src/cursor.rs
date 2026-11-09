//! Port of open-sse/executors/cursor.js.
//!
//! Cursor agent executor: protobuf streaming (ChatService) and AgentService
//! runs with checksum validation. The chat wire is `application/connect+proto`
//! over HTTP/2; upstream is fetched with the shared `proxy_fetch` (HTTP/2 via
//! reqwest) and decoded from Connect frames into OpenAI SSE.

use async_trait::async_trait;
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use nr_core::http::{HeaderList, UpstreamResponse};
use nr_core::{Credentials, Error, ExecRequest, ExecResult, Json, Result};
use nr_exec_base::BaseHooks;
use serde_json::json;
use std::sync::Arc;

use crate::cursor_checksum::build_cursor_headers;
use crate::cursor_models::{resolve_auto_model_selection, resolve_cursor_models};
use crate::cursor_protobuf::{
    decode_message, extract_text_from_response, generate_cursor_body, is_agent_capable_request,
    read_cursor_frame, CursorFrame,
};
use nr_common::sse::chat_chunk_sse;
use nr_common::sse_constants::{SSE_DONE, SSE_HEADERS};

/// AgentService Run endpoint path (JS `AGENT_RUN_PATH`).
const AGENT_RUN_PATH: &str = "/agent.v1.AgentService/Run";
/// `chatcmpl-` prefix so the Claude SSE translator can derive Anthropic's `msg_` id.
fn response_id() -> String {
    format!("chatcmpl-msg_{}", chrono::Utc::now().timestamp_millis())
}

pub struct CursorExecutor {
    provider: String,
    config: Json,
}

impl CursorExecutor {
    pub fn new(provider_id: &str) -> Self {
        let config = nr_data::provider(provider_id).cloned().unwrap_or_default();
        Self {
            provider: provider_id.to_string(),
            config,
        }
    }

    fn chat_url(&self) -> Result<String> {
        let base = self
            .config
            .get("baseUrl")
            .and_then(Json::as_str)
            .ok_or_else(|| Error::Config("Cursor: baseUrl is not configured".into()))?;
        let chat_path = self
            .config
            .get("chatPath")
            .and_then(Json::as_str)
            .ok_or_else(|| Error::Config("Cursor: chatPath is not configured".into()))?;
        Ok(format!("{base}{chat_path}"))
    }

    /// Build the Cursor header set, requiring a machine id (JS throws without it).
    fn cursor_headers(&self, creds: &Credentials) -> Result<HeaderList> {
        let access_token = creds
            .access_token()
            .ok_or_else(|| Error::Config("Cursor: no access token".into()))?;
        let machine_id = creds
            .psd_str("machineId")
            .ok_or_else(|| Error::Config("Cursor: Machine ID is required for Cursor API".into()))?;
        let ghost_mode = creds
            .psd()
            .and_then(|p| p.get("ghostMode"))
            .and_then(Json::as_bool)
            .unwrap_or(true);

        let mut headers = HeaderList::new();
        for (k, v) in build_cursor_headers(access_token, Some(machine_id), ghost_mode) {
            headers.insert(k, v);
        }
        Ok(headers)
    }

    fn force_agent_mode(&self, creds: &Credentials) -> bool {
        let ua = creds
            .psd()
            .and_then(|p| p.get("rawHeaders"))
            .and_then(|h| h.get("user-agent"))
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_lowercase();
        ua.contains("claude-cli") || ua.contains("claude-code") || ua.contains("claude code")
    }
}

#[async_trait]
impl BaseHooks for CursorExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn config(&self) -> &Json {
        &self.config
    }

    fn build_url(&self, _model: &str, _stream: bool, _url_index: usize, _creds: &Credentials) -> Result<String> {
        self.chat_url()
    }

    fn build_headers(
        &self,
        creds: &Credentials,
        _stream: bool,
        _url: &str,
        _model: &str,
        _body: &Json,
    ) -> Result<HeaderList> {
        self.cursor_headers(creds)
    }

    /// JS `transformRequest` returns `generateCursorBody(...)` — a binary
    /// Connect-RPC frame. That cannot ride on `base_execute` (which JSON-encodes
    /// bodies), so the JSON body passes through untouched and [`Self::execute`]
    /// builds the frame itself, like the JS `execute` override.
    fn transform_request(&self, _model: &str, body: &Json, _stream: bool, _creds: &Credentials) -> Result<Json> {
        Ok(body.clone())
    }
}

fn json_error(status: u16, message: &str, kind: &str) -> UpstreamResponse {
    let payload = json!({ "error": { "message": message, "type": kind } });
    UpstreamResponse::json(status, &payload)
}

/// Fetch the chat endpoint and return the buffered response body.
async fn fetch_chat(
    url: &str,
    headers: HeaderList,
    body: &[u8],
    req: &ExecRequest<'_>,
) -> Result<UpstreamResponse> {
    let mut fetch = nr_core::http::FetchRequest::new(http::Method::POST, url.to_string());
    fetch.headers = headers;
    fetch.body = Some(Bytes::from(body.to_vec()));
    fetch.header_timeout = Some(std::time::Duration::from_secs(60));
    fetch.cancel = Some(req.cancel.clone());
    nr_core::http::proxy_fetch(fetch, Some(&req.proxy)).await
}

/// Decode a Connect frame stream into one OpenAI JSON response.
fn transform_protobuf_to_json(buffer: &[u8], model: &str, body: &Json) -> Result<UpstreamResponse> {
    let id = format!("chatcmpl-cursor-{}", chrono::Utc::now().timestamp_millis());
    let created = chrono::Utc::now().timestamp();
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<Json> = Vec::new();
    let mut finish_reason = "stop";
    let error: Option<String> = None;

    let mut offset = 0;
    loop {
        match read_cursor_frame(buffer, offset) {
            CursorFrame::Done => break,
            CursorFrame::Skip(next) => {
                offset = next;
                continue;
            }
            CursorFrame::Ok(payload, next) => {
                offset = next;
                if payload.first() == Some(&0x7b) {
                    if let Ok(text) = std::str::from_utf8(&payload) {
                        if text.contains("\"error\"") {
                            if content.is_empty() && tool_calls.is_empty() {
                                if let Ok(parsed) = serde_json::from_str::<Json>(text) {
                                    let msg = parsed
                                        .pointer("/error/message")
                                        .and_then(Json::as_str)
                                        .unwrap_or("API Error");
                                    return Ok(json_error(400, msg, "api_error"));
                                }
                            }
                            break;
                        }
                    }
                }
                let extracted = extract_text_from_response(&payload);
                if let Some(text) = extracted.text {
                    content.push_str(&text);
                }
                if let Some(thinking) = extracted.thinking {
                    reasoning.push_str(&thinking);
                }
                if let Some(tc) = extracted.tool_call {
                    tool_calls.push(json!({
                        "id": tc.id,
                        "type": "function",
                        "function": { "name": tc.name, "arguments": tc.arguments },
                    }));
                    finish_reason = "tool_calls";
                }
                let _ = error;
            }
        }
    }

    if let Some(err) = error {
        return Ok(json_error(400, &err, "api_error"));
    }

    let usage = nr_common::usage_tracking::estimate_usage(body, content.len() as i64, "openai");
    let mut message = json!({ "role": "assistant", "content": if content.is_empty() { Json::Null } else { Json::String(content.clone()) } });
    if !reasoning.is_empty() {
        message["reasoning_content"] = Json::String(reasoning);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = Json::Array(tool_calls);
    }
    let payload = json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{ "index": 0, "message": message, "finish_reason": finish_reason }],
        "usage": usage,
    });
    Ok(UpstreamResponse::json(200, &payload))
}

/// Decode a Connect frame stream into an OpenAI SSE stream.
fn transform_protobuf_to_sse(buffer: Vec<u8>, model: String) -> UpstreamResponse {
    let id = response_id();
    let created = chrono::Utc::now().timestamp();
    let (mut tx, rx) = futures::channel::mpsc::channel::<std::io::Result<Bytes>>(32);

    tokio::spawn(async move {
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut tool_calls: Vec<Json> = Vec::new();
        let mut finish_reason = "stop";
        let mut errored = false;
        let mut offset = 0;

        loop {
            match read_cursor_frame(&buffer, offset) {
                CursorFrame::Done => break,
                CursorFrame::Skip(next) => {
                    offset = next;
                    continue;
                }
                CursorFrame::Ok(payload, next) => {
                    offset = next;
                    if payload.first() == Some(&0x7b) {
                        if let Ok(text) = std::str::from_utf8(&payload) {
                            if text.contains("\"error\"") {
                                if let Ok(parsed) = serde_json::from_str::<Json>(text) {
                                    let msg = parsed
                                        .pointer("/error/message")
                                        .and_then(Json::as_str)
                                        .unwrap_or("API Error");
                                    let frame = nr_common::sse::sse_chunk(&json!({
                                        "error": { "message": msg, "type": "api_error" },
                                    }));
                                    let _ = tx.send(Ok(Bytes::from(frame))).await;
                                    errored = true;
                                }
                                break;
                            }
                        }
                    }
                    let extracted = extract_text_from_response(&payload);
                    if let Some(text) = extracted.text {
                        content.push_str(&text);
                        let frame = chat_chunk_sse(&id, created, &model, &json!({ "content": text }), None);
                        let _ = tx.send(Ok(Bytes::from(frame))).await;
                    }
                    if let Some(thinking) = extracted.thinking {
                        reasoning.push_str(&thinking);
                        let frame =
                            chat_chunk_sse(&id, created, &model, &json!({ "reasoning_content": thinking }), None);
                        let _ = tx.send(Ok(Bytes::from(frame))).await;
                    }
                    if let Some(tc) = extracted.tool_call {
                        tool_calls.push(json!({
                            "id": tc.id,
                            "type": "function",
                            "function": { "name": tc.name, "arguments": tc.arguments },
                        }));
                        finish_reason = "tool_calls";
                        let frame = chat_chunk_sse(
                            &id,
                            created,
                            &model,
                            &json!({ "tool_calls": [json!({
                                "index": 0, "id": tc.id, "type": "function",
                                "function": { "name": tc.name, "arguments": tc.arguments },
                            })] }),
                            None,
                        );
                        let _ = tx.send(Ok(Bytes::from(frame))).await;
                    }
                }
            }
        }

        if !errored {
            let frame = chat_chunk_sse(&id, created, &model, &json!({}), Some(finish_reason));
            let _ = tx.send(Ok(Bytes::from(frame))).await;
            let _ = tx.send(Ok(Bytes::from(SSE_DONE))).await;
        }
    });

    let mut headers = http::HeaderMap::new();
    for (k, v) in SSE_HEADERS {
        if let (Ok(name), Ok(value)) = (
            http::header::HeaderName::try_from(*k),
            http::HeaderValue::try_from(*v),
        ) {
            headers.insert(name, value);
        }
    }
    UpstreamResponse::from_stream(200, headers, Box::pin(rx))
}

/// AgentService endpoint URL from PROVIDER_OAUTH.cursor.
fn agent_run_url() -> Result<String> {
    let config = nr_data::provider_oauth("cursor")
        .ok_or_else(|| Error::Config("Cursor AgentService endpoint is not configured".into()))?;
    let agent_endpoint = config
        .get("agentEndpoint")
        .and_then(Json::as_str)
        .ok_or_else(|| Error::Config("Cursor AgentService endpoint is not configured".into()))?;
    Ok(format!("{}{AGENT_RUN_PATH}", agent_endpoint.trim_end_matches('/')))
}

/// Decode an AgentService stream into OpenAI SSE, resolving the auto model first.
async fn execute_agent(ex: &CursorExecutor, req: &ExecRequest<'_>) -> Result<ExecResult> {
    let url = agent_run_url()?;
    let headers = ex.cursor_headers(req.credentials)?;

    let empty = Vec::new();
    let messages = req.body.get("messages").and_then(Json::as_array).unwrap_or(&empty);
    let tools = req.body.get("tools").and_then(Json::as_array).unwrap_or(&empty);
    let reasoning_effort = req
        .body
        .get("reasoning_effort")
        .and_then(Json::as_str)
        .or_else(|| req.body.get("reasoning").and_then(|r| r.get("effort")).and_then(Json::as_str));

    // "Auto (Server Picks)" / "default" is a picker alias, not a Run model id.
    let mut run_model = req.model.to_string();
    if req.model == "default" || req.model == "auto" {
        if let Ok(Some(catalog)) = resolve_cursor_models(req.credentials, false, Some(&req.proxy)).await {
            if let Some(models) = catalog.get("models").and_then(Json::as_array) {
                if let Some(selection) = resolve_auto_model_selection(models) {
                    if let Some(id) = selection.get("modelId").and_then(Json::as_str) {
                        run_model = id.to_string();
                    }
                }
            }
        }
    }

    let frame = crate::cursor_protobuf::build_agent_run_frame(messages, &run_model, tools, reasoning_effort);
    let mut fetch = nr_core::http::FetchRequest::new(http::Method::POST, url.clone());
    fetch.headers = headers.clone();
    fetch.body = Some(Bytes::from(frame));
    fetch.header_timeout = Some(std::time::Duration::from_secs(60));
    fetch.cancel = Some(req.cancel.clone());
    let response = nr_core::http::proxy_fetch(fetch, Some(&req.proxy)).await?;

    let status = response.status;
    if status != 200 {
        let text = response.text().await.unwrap_or_default();
        return Ok(ExecResult {
            response: json_error(
                if status == 0 { 500 } else { status },
                &format!("Cursor AgentService {status}: {}", if text.is_empty() { "request failed".to_string() } else { text }),
                "api_error",
            ),
            url,
            headers,
            transformed_body: req.body.clone(),
            response_format: Some("openai".into()),
        });
    }

    let buffer = response.bytes().await?.to_vec();
    // AgentService answers one framed run; a long replay can come back as an
    // empty turn, which surfaces as an SSE error frame rather than content.
    let sse = transform_agent_frames_to_sse(buffer, req.model.to_string());
    Ok(ExecResult {
        response: sse,
        url,
        headers,
        transformed_body: req.body.clone(),
        response_format: Some("openai".into()),
    })
}

/// Decode AgentService server frames (agent.v1.AgentServerMessage) into OpenAI SSE.
fn transform_agent_frames_to_sse(buffer: Vec<u8>, model: String) -> UpstreamResponse {
    let id = response_id();
    let created = chrono::Utc::now().timestamp();
    let (mut tx, rx) = futures::channel::mpsc::channel::<std::io::Result<Bytes>>(32);

    tokio::spawn(async move {
        let (frames, _pending) = crate::cursor_protobuf::decode_agent_frames(&buffer);
        let mut thinking_acc = String::new();
        let mut emitted_visible = 0usize;
        let mut emitted_text = false;
        let composer = crate::cursor_protobuf::is_composer_model(&model);
        let mut finished = false;

        for payload in frames {
            if finished {
                break;
            }
            let server_message = decode_message(&payload);

            if let Some(updates) = server_message.get(&1) {
                for update_field in updates {
                    let update = decode_message(&update_field.value);
                    if let Some(text_delta) = update
                        .get(&1)
                        .and_then(|v| v.first())
                        .map(|f| String::from_utf8_lossy(&f.value).to_string())
                        .filter(|s| !s.is_empty())
                    {
                        emitted_text = true;
                        let frame = chat_chunk_sse(&id, created, &model, &json!({ "content": text_delta }), None);
                        if tx.send(Ok(Bytes::from(frame))).await.is_err() {
                            return;
                        }
                    }
                    // thinking_delta (field 4). Composer (and some Grok variants) put
                    // the visible answer after </think> and never send text_delta.
                    if let Some(thinking_delta) = update
                        .get(&4)
                        .and_then(|v| v.first())
                        .map(|f| String::from_utf8_lossy(&f.value).to_string())
                        .filter(|s| !s.is_empty())
                    {
                        thinking_acc.push_str(&thinking_delta);
                        if composer {
                            let visible = crate::cursor_protobuf::visible_composer_content_from_thinking(&thinking_acc);
                            if visible.len() > emitted_visible {
                                let delta = visible[emitted_visible..].to_string();
                                emitted_visible = visible.len();
                                emitted_text = true;
                                let frame = chat_chunk_sse(&id, created, &model, &json!({ "content": delta }), None);
                                if tx.send(Ok(Bytes::from(frame))).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    // Field 14: reasoning signature — Anthropic-only, unsigned here.
                    if update.contains_key(&14) {
                        finished = true;
                    }
                }
            }
        }

        if !emitted_text && !thinking_acc.is_empty() {
            let fallback = if composer {
                crate::cursor_protobuf::visible_composer_content_from_thinking(&thinking_acc)
            } else {
                thinking_acc.trim().to_string()
            };
            if !fallback.is_empty() {
                emitted_text = true;
                let frame = chat_chunk_sse(&id, created, &model, &json!({ "content": fallback }), None);
                let _ = tx.send(Ok(Bytes::from(frame))).await;
            }
        }

        if emitted_text {
            let frame = chat_chunk_sse(&id, created, &model, &json!({}), Some("stop"));
            let _ = tx.send(Ok(Bytes::from(frame))).await;
            let _ = tx.send(Ok(Bytes::from(SSE_DONE))).await;
        } else {
            let frame = nr_common::sse::sse_chunk(&json!({
                "error": { "message": "Cursor AgentService returned an empty turn", "type": "api_error" },
            }));
            let _ = tx.send(Ok(Bytes::from(frame))).await;
            let _ = tx.send(Ok(Bytes::from(SSE_DONE))).await;
        }
    });

    let mut headers = http::HeaderMap::new();
    for (k, v) in SSE_HEADERS {
        if let (Ok(name), Ok(value)) = (
            http::header::HeaderName::try_from(*k),
            http::HeaderValue::try_from(*v),
        ) {
            headers.insert(name, value);
        }
    }
    UpstreamResponse::from_stream(200, headers, Box::pin(rx))
}

#[async_trait]
impl nr_core::Executor for CursorExecutor {
    fn provider(&self) -> &str {
        BaseHooks::provider(self)
    }

    fn no_auth(&self) -> bool {
        BaseHooks::no_auth(self)
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        // AgentService handles text-only turns; image parts need the legacy
        // protobuf chat path.
        if is_agent_capable_request(&req.body) {
            match execute_agent(self, &req).await {
                Ok(result) => return Ok(result),
                Err(err) => {
                    let url = agent_run_url().unwrap_or_default();
                    return Ok(ExecResult {
                        response: json_error(500, &err.to_string(), "connection_error"),
                        url,
                        headers: HeaderList::new(),
                        transformed_body: req.body.clone(),
                        response_format: None,
                    });
                }
            }
        }

        let url = self.build_url(req.model, req.stream, 0, req.credentials)?;
        let headers = self.build_headers(req.credentials, req.stream, &url, req.model, &req.body)?;

        // Messages are already translated by chatCore (claude→openai→cursor).
        // Re-running openaiToCursorRequest would double-translate and drop tool_results.
        let empty = Vec::new();
        let messages = req.body.get("messages").and_then(Json::as_array).unwrap_or(&empty);
        let tools = req.body.get("tools").and_then(Json::as_array).unwrap_or(&empty);
        let reasoning_effort = req
            .body
            .get("reasoning_effort")
            .and_then(Json::as_str)
            .or_else(|| req.body.get("reasoning").and_then(|r| r.get("effort")).and_then(Json::as_str));
        let body_bytes = generate_cursor_body(
            messages,
            req.model,
            tools,
            reasoning_effort,
            self.force_agent_mode(req.credentials),
        );

        let response = match fetch_chat(&url, headers.clone(), &body_bytes, &req).await {
            Ok(response) => response,
            Err(err) => {
                return Ok(ExecResult {
                    response: json_error(500, &err.to_string(), "connection_error"),
                    url,
                    headers,
                    transformed_body: req.body.clone(),
                    response_format: None,
                });
            }
        };

        let status = response.status;
        if status != 200 {
            let text = response.text().await.unwrap_or_default();
            return Ok(ExecResult {
                response: json_error(
                    status,
                    &format!("[{status}]: {}", if text.is_empty() { "Unknown error".to_string() } else { text }),
                    "invalid_request_error",
                ),
                url,
                headers,
                transformed_body: req.body.clone(),
                response_format: None,
            });
        }

        let buffer = response.bytes().await?;
        let upstream = if req.stream {
            transform_protobuf_to_sse(buffer.to_vec(), req.model.to_string())
        } else {
            transform_protobuf_to_json(&buffer, req.model, &req.body)?
        };

        Ok(ExecResult {
            response: upstream,
            url,
            headers,
            transformed_body: req.body.clone(),
            response_format: Some("openai".into()),
        })
    }
}

pub fn create(provider: &str) -> Arc<dyn nr_core::Executor> {
    Arc::new(CursorExecutor::new(provider))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_chat_url() {
        let ex = CursorExecutor::new("cursor");
        assert_eq!(
            ex.chat_url().unwrap(),
            "https://api2.cursor.sh/aiserver.v1.ChatService/StreamUnifiedChatWithTools"
        );
    }

    #[test]
    fn requires_machine_id() {
        let ex = CursorExecutor::new("cursor");
        let creds = Credentials::from_value(json!({"accessToken": "tok"}));
        let err = ex.cursor_headers(&creds).unwrap_err();
        assert!(err.to_string().contains("Machine ID"));
    }

    #[test]
    fn builds_checksum_headers() {
        let ex = CursorExecutor::new("cursor");
        let creds = Credentials::from_value(json!({
            "accessToken": "tok",
            "providerSpecificData": { "machineId": "machine", "ghostMode": true },
        }));
        let headers = ex.cursor_headers(&creds).unwrap();
        assert_eq!(headers["authorization"], "Bearer tok");
        assert!(headers["x-cursor-checksum"].ends_with("machine"));
        assert_eq!(headers["content-type"], "application/connect+proto");
    }

    #[test]
    fn detects_agent_capable_requests() {
        assert!(is_agent_capable_request(&json!({
            "messages": [
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": [{ "type": "text", "text": "hello" }] },
            ],
        })));
        assert!(!is_agent_capable_request(&json!({
            "messages": [{ "role": "user", "content": [{ "type": "image_url", "image_url": {} }] }],
        })));
        assert!(!is_agent_capable_request(&json!({ "messages": [] })));
    }

    #[test]
    fn forces_agent_mode_on_claude_cli() {
        let ex = CursorExecutor::new("cursor");
        let creds = Credentials::from_value(json!({
            "providerSpecificData": { "rawHeaders": { "user-agent": "claude-cli/1.0" } },
        }));
        assert!(ex.force_agent_mode(&creds));
        let plain = Credentials::from_value(json!({}));
        assert!(!ex.force_agent_mode(&plain));
    }

    #[test]
    fn transform_request_passes_body_through() {
        let ex = CursorExecutor::new("cursor");
        let creds = Credentials::from_value(json!({}));
        let body = json!({ "messages": [{ "role": "user", "content": "hello" }] });
        // The binary Connect frame is built in `execute`; the JSON body is untouched.
        assert_eq!(ex.transform_request("composer-1", &body, true, &creds).unwrap(), body);
    }

    #[test]
    fn chat_body_is_a_connect_frame() {
        let bytes = generate_cursor_body(
            &[json!({ "role": "user", "content": "hello" })],
            "composer-1",
            &[],
            Some("high"),
            false,
        );
        // Connect frame: flags byte 0x00, then a 4-byte big-endian length.
        assert_eq!(bytes[0], 0x00);
        let length = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) as usize;
        assert_eq!(bytes.len(), 5 + length);
        assert!(length > 0);
    }
}
