//! Port of open-sse/executors/qoder.js.
//!
//! QoderExecutor sends OpenAI-format chat requests to Qoder's COSY-signed
//! inference endpoint at api3.qoder.sh, then unwraps Qoder's
//! `{statusCodeValue, body}` SSE envelope back into plain OpenAI SSE for the
//! rest of the pipeline.
//!
//! Differences vs a plain OpenAI-compatible endpoint:
//!   - URL is `api3.qoder.sh/algo/api/v2/service/pro/sse/agent_chat_generation`
//!     with `&Encode=1` so the body can ship through the WAF-bypass encoder.
//!   - Authentication is COSY (RSA + AES + MD5 + ~17 Cosy-* headers), not a
//!     static bearer token.
//!   - The request shape Qoder expects is non-trivial (chat_context with a
//!     mirrored modelConfig, business block with stable IDs, system text
//!     hoisted out of the messages array).
//!   - Model identifier is one of the canonical Qoder keys; the translator
//!     layer feeds us "qoder/<key>" so the prefix is stripped.
//!   - Per-model `model_config` is fetched live from /algo/api/v2/model/list
//!     and cached. Sending the wrong block silently downgrades to a different
//!     model upstream, so a missing entry is a hard error.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use nr_core::http::{proxy_fetch, FetchRequest, HeaderList, UpstreamResponse};
use nr_core::{Credentials, Error, ExecRequest, ExecResult, Executor, Json, Logger, Result};
use serde_json::json;
use sha2::{Digest, Sha256};

use self::constants::{qoder_inference_base, qoder_region_of, qoder_static_model_config, QODER_CHAT_SIG_PATH};
use self::context_tier::{apply_context_tier, env_tier_preference, resolve_context_tier};
use self::cosy::qoder_encode_body;
use self::models::{
    cosy_headers_for, get_qoder_model_config, is_qoder_pat, resolve_qoder_credentials,
    resolve_qoder_models,
};
use self::queue::{
    compute_qoder_queue_delay_ms, parse_qoder_queue_info, parse_qoder_queue_status,
    qoder_queue_max_wait_ms, qoder_queue_status_url,
};
use self::sse::{is_billing_block, unwrap_envelope_line, EnvelopeLine, QoderSseCoalescer};

pub mod attachments;
pub mod constants;
pub mod context_tier;
pub mod cosy;
pub mod models;
pub mod queue;
pub mod sse;

const HTTP_BAD_GATEWAY: u16 = 502;
const HTTP_FORBIDDEN: u16 = 403;
const HTTP_BAD_REQUEST: u16 = 400;

/// `buildUrl(credentials)` — inference endpoint with the encoder flag.
pub fn build_chat_url(credentials: &Credentials, region: &str) -> String {
    format!(
        "{}/algo{QODER_CHAT_SIG_PATH}?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1",
        qoder_inference_base(&credentials.clone().into_value(), region)
    )
}

/// `extractText(content)`.
fn extract_text(content: &Json) -> String {
    match content {
        Json::String(s) => s.clone(),
        Json::Null => String::new(),
        Json::Array(items) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Json::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        other => match other {
            Json::Number(n) => n.to_string(),
            Json::Bool(b) => b.to_string(),
            _ => String::new(),
        },
    }
}

/// `normalizeContent(content)` — flatten to a string unless images are present.
pub fn normalize_content(content: &Json) -> Json {
    match content {
        Json::String(s) => Json::String(s.clone()),
        Json::Null => Json::String(String::new()),
        Json::Array(items) => {
            let mut blocks: Vec<Json> = Vec::new();
            let mut text_parts: Vec<String> = Vec::new();
            let mut has_image = false;

            let mut push_text = |text: &str, blocks: &mut Vec<Json>, text_parts: &mut Vec<String>, has_image: &mut bool| {
                if text.is_empty() {
                    return;
                }
                if *has_image || !blocks.is_empty() {
                    blocks.push(json!({ "type": "text", "text": text }));
                } else {
                    text_parts.push(text.to_string());
                }
            };

            for item in items {
                if !item.is_object() {
                    continue;
                }
                let item_type = item.get("type").and_then(Json::as_str).unwrap_or("");

                let image_url: Option<&str> = if item_type == "image_url" {
                    match item.get("image_url") {
                        Some(Json::String(u)) if !u.is_empty() => Some(u.as_str()),
                        Some(v) => v.get("url").and_then(Json::as_str).filter(|u| !u.is_empty()),
                        None => None,
                    }
                } else {
                    None
                };
                if let Some(url) = image_url {
                    blocks.push(json!({ "type": "image_url", "image_url": { "url": url } }));
                    has_image = true;
                    continue;
                }

                if item_type == "image" && item.get("source").is_some() {
                    let src = item.get("source").unwrap_or(&Json::Null);
                    let url = if src.get("type").and_then(Json::as_str) == Some("base64") {
                        src.get("data").and_then(Json::as_str).filter(|d| !d.is_empty()).map(|data| {
                            let mime = src.get("media_type").and_then(Json::as_str).unwrap_or("image/png");
                            nr_translator::concerns::image::encode_data_uri(mime, data)
                        })
                    } else {
                        src.get("url").and_then(Json::as_str).filter(|u| !u.is_empty()).map(String::from)
                    };
                    if let Some(url) = url {
                        blocks.push(json!({ "type": "image_url", "image_url": { "url": url } }));
                        has_image = true;
                    }
                    continue;
                }

                if item_type == "file" {
                    let name = item
                        .get("file")
                        .and_then(|f| f.get("filename").or_else(|| f.get("name")))
                        .and_then(Json::as_str)
                        .unwrap_or("file");
                    push_text(
                        &format!("[file omitted: {name} — Qoder reads documents via its file API, not inlined bytes]"),
                        &mut blocks,
                        &mut text_parts,
                        &mut has_image,
                    );
                    continue;
                }

                if item_type == "document" {
                    let name = item.get("title").and_then(Json::as_str).unwrap_or("document");
                    push_text(
                        &format!("[file omitted: {name} — Qoder reads documents via its file API, not inlined bytes]"),
                        &mut blocks,
                        &mut text_parts,
                        &mut has_image,
                    );
                    continue;
                }

                if let Some(text) = item.get("text").and_then(Json::as_str).filter(|t| !t.is_empty()) {
                    push_text(text, &mut blocks, &mut text_parts, &mut has_image);
                }
            }

            if !has_image {
                return Json::String(text_parts.join("\n"));
            }
            // Prepend any text collected before the first image block.
            if !text_parts.is_empty() {
                blocks.insert(0, json!({ "type": "text", "text": text_parts.join("\n") }));
            }
            Json::Array(blocks)
        }
        Json::Number(n) => Json::String(n.to_string()),
        Json::Bool(b) => Json::String(b.to_string()),
        // JS `String({})` is "[object Object]".
        Json::Object(_) => Json::String("[object Object]".into()),
    }
}

/// `normalizeMessages(messages)` — hoist system messages out (Qoder rejects
/// `role: "system"` inside the array) and normalize content blocks.
pub fn normalize_messages(messages: &Json) -> (Vec<Json>, String) {
    let Some(list) = messages.as_array() else { return (Vec::new(), String::new()) };
    if list.is_empty() {
        return (Vec::new(), String::new());
    }
    let mut system_parts: Vec<String> = Vec::new();
    let mut out: Vec<Json> = Vec::new();
    for msg in list {
        if !msg.is_object() {
            continue;
        }
        if msg.get("role").and_then(Json::as_str) == Some("system") {
            let text = extract_text(msg.get("content").unwrap_or(&Json::Null));
            if !text.is_empty() {
                system_parts.push(text);
            }
            continue;
        }
        let mut cloned = msg.clone();
        let content = msg.get("content").cloned().unwrap_or(Json::Null);
        cloned["content"] = normalize_content(&content);
        out.push(cloned);
    }
    (out, system_parts.join("\n\n"))
}

fn last_user_text(messages: &[Json]) -> String {
    for msg in messages.iter().rev() {
        if msg.get("role").and_then(Json::as_str) != Some("user") {
            continue;
        }
        return match msg.get("content") {
            Some(Json::String(s)) => s.clone(),
            Some(Json::Array(_)) => extract_text(msg.get("content").unwrap_or(&Json::Null)),
            _ => String::new(),
        };
    }
    String::new()
}

fn stable_hash_16(prefix: &str, parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prefix.as_bytes());
    for p in parts {
        hasher.update([0u8]);
        hasher.update(p.as_bytes());
    }
    format!("{:x}", hasher.finalize())[..16].to_string()
}

fn stable_chat_record_id(model: &str, messages: &[Json], tools: Option<&Json>, max_tokens: i64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"qoder-record\0");
    hasher.update(model.as_bytes());
    for m in messages {
        if let Some(role) = m.get("role").and_then(Json::as_str) {
            hasher.update([0u8]);
            hasher.update(role.as_bytes());
        }
        match m.get("content") {
            Some(Json::String(s)) if !s.is_empty() => {
                hasher.update([0u8]);
                hasher.update(s.as_bytes());
            }
            Some(Json::Array(_)) => {
                // Include image refs so the same prompt with a different image
                // gets a distinct chat_record_id.
                hasher.update([0u8]);
                hasher.update(m.get("content").unwrap_or(&Json::Null).to_string().as_bytes());
            }
            _ => {}
        }
    }
    if let Some(tools) = tools {
        hasher.update([0u8]);
        hasher.update(tools.to_string().as_bytes());
    }
    hasher.update(format!("\0mt={max_tokens}").as_bytes());
    format!("{:x}", hasher.finalize())[..16].to_string()
}

fn truncate_chars(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        let head: String = s.chars().take(n).collect();
        format!("{head}...")
    } else {
        s.to_string()
    }
}

/// Everything [`build_request_body`] produces.
pub struct BuiltBody {
    pub qoder_key: String,
    pub payload: Json,
    pub model_config: Json,
}

/// `buildQoderRequestBody({ model, body, credentials, ... })`.
///
/// The attachment rewrite happens before this is called (it needs async I/O).
pub async fn build_request_body(
    model: &str,
    body: &Json,
    credentials: &Credentials,
    log: &Logger,
    proxy: Option<&nr_core::http::ProxyOptions>,
    region: &str,
) -> Result<BuiltBody> {
    let qoder_key = model.strip_prefix("qoder/").unwrap_or(model).to_string();

    // Fetch the model config from the live API instead of relying on the
    // static map, so new Qoder models work without a code change.
    let mut model_config = get_qoder_model_config(credentials, &qoder_key, proxy, region, log).await;
    if model_config.is_none() {
        // Try a forced refresh once before giving up — the cache may simply not
        // be populated yet on the first ever call for this credential.
        if let Some(refreshed) = resolve_qoder_models(credentials, proxy, region, true, log).await {
            model_config = refreshed.raw_configs.get(&qoder_key).map(|cfg| {
                let mut cfg = cfg.clone();
                cfg["key"] = Json::String(qoder_key.clone());
                cfg
            });
        }
    }
    let model_config = match model_config {
        Some(cfg) => cfg,
        None => {
            // Last resort: a static block for models the per-account catalog
            // omits. Sonus (smodel) / Cantus (cmodel) are the known cases — the
            // chat endpoint accepts them even when /model/list never mentions
            // them, so refusing here would make an available model look broken.
            match qoder_static_model_config(&qoder_key) {
                Some(cfg) => {
                    log.info(
                        "QODER",
                        format!("using static model_config for \"{qoder_key}\" (absent from the account catalog)"),
                    );
                    cfg
                }
                None => {
                    return Err(Error::BadRequest(format!(
                        "qoder: model_config for \"{qoder_key}\" not yet known (run a model list fetch or check upstream connectivity)"
                    )))
                }
            }
        }
    };

    let incoming = body.get("messages").cloned().unwrap_or(Json::Array(Vec::new()));
    let (messages, system_text) = normalize_messages(&incoming);
    let tools = body.get("tools").cloned().unwrap_or(Json::Null);
    let is_reasoning = model_config.get("is_reasoning") == Some(&Json::Bool(true));
    let max_output = model_config.get("max_output_tokens").and_then(Json::as_i64).unwrap_or(0);

    let mut max_tokens: i64 = 32_768;
    if max_output > 0 {
        max_tokens = max_output;
    }
    for key in ["max_tokens", "max_completion_tokens"] {
        if let Some(v) = body.get(key).and_then(Json::as_i64) {
            if v > 0 && v < max_tokens {
                max_tokens = v;
            }
        }
    }

    let last_user = last_user_text(&messages);
    let user_id = credentials.psd_str("userId").unwrap_or("");
    let session_id = stable_hash_16("qoder-session", &[user_id, &qoder_key]);
    let record_id = stable_chat_record_id(&qoder_key, &messages, Some(&tools), max_tokens);

    // Context-window tier (200K/400K/1M): the IDE picks one from
    // model_config.context_config; qodercli-style requests default to the
    // smallest. Escalate when the prompt no longer fits.
    let preference = env_tier_preference();
    let tier_choice = resolve_context_tier(
        &model_config,
        &system_text,
        &Json::Array(messages.clone()),
        &tools,
        preference.as_deref(),
    );
    if let Some(choice) = &tier_choice {
        log.info(
            "QODER",
            format!(
                "context tier {} ({} tokens, {}) for ~{} prompt tokens",
                choice.tier.name, choice.tier.token_count, choice.reason, choice.estimated_tokens
            ),
        );
    }

    let mut payload = json!({
        "request_id": uuid::Uuid::new_v4().to_string(),
        "request_set_id": record_id,
        "chat_record_id": record_id,
        "session_id": session_id,
        "stream": true,
        "chat_task": "FREE_INPUT",
        "is_reply": true,
        "is_retry": false,
        "source": 1,
        "version": "3",
        "session_type": "qodercli",
        "agent_id": "agent_common",
        "task_id": "common",
        "code_language": "",
        "chat_prompt": "",
        "image_urls": Json::Null,
        "aliyun_user_type": "",
        "system": system_text,
        "messages": Json::Array(messages),
        "tools": if tools.is_array() { tools.clone() } else { Json::Array(Vec::new()) },
        "parameters": { "max_tokens": max_tokens },
        "chat_context": {
            "chatPrompt": "",
            "imageUrls": Json::Null,
            "extra": {
                "context": [],
                "modelConfig": { "key": qoder_key, "is_reasoning": is_reasoning },
                "originalContent": last_user,
            },
            "features": [],
            "text": last_user,
        },
        "model_config": model_config.clone(),
        "business": {
            "product": "cli",
            "version": "1.0.0",
            "type": "agent",
            "stage": "start",
            "id": uuid::Uuid::new_v4().to_string(),
            "name": truncate_chars(&last_user, 30),
            "begin_at": chrono::Utc::now().timestamp_millis(),
        },
    });
    if let Some(choice) = &tier_choice {
        apply_context_tier(&mut payload, &choice.tier);
    }

    Ok(BuiltBody { qoder_key, payload, model_config })
}

fn fake_json_response(status: u16, message: &str) -> UpstreamResponse {
    UpstreamResponse::json(status, &json!({ "error": { "message": message } }))
}

/// Byte-buffered line scanner over the upstream body.
///
/// `consumed` keeps every byte read so far (like the JS peek), which lets the
/// wrapper re-process the peeked frame instead of re-sending the request.
struct LineReader {
    stream: nr_core::http::BodyStream,
    consumed: String,
    scan: usize,
    upstream_done: bool,
}

impl LineReader {
    fn new(stream: nr_core::http::BodyStream) -> Self {
        Self { stream, consumed: String::new(), scan: 0, upstream_done: false }
    }

    /// Peek the first `data:` frame without dropping any bytes.
    async fn peek_first_frame(&mut self) -> (Option<EnvelopeLine>, bool) {
        loop {
            if let Some(nl) = self.consumed[self.scan..].find('\n') {
                let line = self.consumed[self.scan..self.scan + nl].to_string();
                self.scan += nl + 1;
                match unwrap_envelope_line(&line) {
                    EnvelopeLine::Skip => {
                        if self.scan >= self.consumed.len() {
                            if self.upstream_done {
                                return (None, true);
                            }
                            self.pull().await;
                        }
                        continue;
                    }
                    other => return (Some(other), self.upstream_done),
                }
            }
            if self.upstream_done {
                let line = self.consumed[self.scan..].to_string();
                self.scan = self.consumed.len();
                if line.trim().is_empty() {
                    return (None, true);
                }
                return (Some(unwrap_envelope_line(&line)), true);
            }
            self.pull().await;
        }
    }

    async fn pull(&mut self) {
        match self.stream.next().await {
            Some(Ok(bytes)) => self.consumed.push_str(&String::from_utf8_lossy(&bytes)),
            Some(Err(_)) | None => self.upstream_done = true,
        }
    }
}

/// Split one already-buffered chunk into complete lines.
fn drain_lines(buffer: &mut String) -> Vec<String> {
    let mut out = Vec::new();
    while let Some(nl) = buffer.find('\n') {
        let line: String = buffer[..nl].to_string();
        buffer.drain(..=nl);
        out.push(line);
    }
    out
}

/// `wrapQoderSSE` — re-emit each envelope's inner OpenAI chunk as plain SSE.
///
/// Critical: Qoder's SSE often keeps the socket open after the terminal
/// [DONE]/error frame (agent keepalive). Non-streaming clients drain via
/// `response.text()` which then hangs, so on a terminal event the upstream
/// reader is dropped and the stream closed immediately.
pub fn wrap_sse(reader: LineReader, model: &str) -> UpstreamResponse {
    let (tx, rx) = tokio::sync::mpsc::channel::<std::result::Result<Bytes, std::io::Error>>(16);
    let model = model.to_string();
    // Re-process every byte the peek consumed, then continue from the socket.
    let mut buffer = reader.consumed[reader.scan.min(reader.consumed.len())..].to_string();
    let mut stream = reader.stream;
    let mut upstream_done = reader.upstream_done;

    tokio::spawn(async move {
        let mut coalescer = QoderSseCoalescer::new(model.clone());
        let mut terminal = false;

        // Returns (bytes to emit, terminal).
        fn process_line(line: &str, model: &str, coalescer: &mut QoderSseCoalescer) -> (String, bool) {
            let mut out = String::new();
            match unwrap_envelope_line(line) {
                EnvelopeLine::Skip => (out, false),
                EnvelopeLine::Done => {
                    coalescer.flush(&mut out);
                    (out, true)
                }
                EnvelopeLine::Frame(inner) => {
                    let done = coalescer.handle_inner(&inner, &mut out);
                    (out, done)
                }
                EnvelopeLine::Error { status, message } => {
                    if is_billing_block(&message) {
                        // Billing/quota envelope at any stream position: emit a
                        // structured error chunk so chatCore locks the model and
                        // falls back instead of recording fake assistant text.
                        let err_obj = json!({
                            "error": {
                                "message": if message.is_empty() { format!("qoder billing block ({status})") } else { message },
                                "code": "qoder_billing_block",
                                "status": 403,
                                "type": "quota_error",
                            }
                        });
                        out.push_str(&format!("data: {err_obj}\n\ndata: [DONE]\n\n"));
                    } else {
                        let msg = if message.is_empty() { format!("upstream status {status}") } else { message };
                        let err_chunk = json!({
                            "id": format!("qoder-error-{}", chrono::Utc::now().timestamp_millis()),
                            "object": "chat.completion.chunk",
                            "created": chrono::Utc::now().timestamp(),
                            "model": model,
                            "choices": [{
                                "index": 0,
                                "delta": { "content": format!("\n[qoder error {status}: {}]", truncate_chars(&msg, 200)) },
                                "finish_reason": "stop"
                            }],
                        });
                        out.push_str(&format!("data: {err_chunk}\n\ndata: [DONE]\n\n"));
                    }
                    (out, true)
                }
            }
        }

        // Drain whatever the peek already pulled off the socket first.
        for line in drain_lines(&mut buffer) {
            let (out, done) = process_line(&line, &model, &mut coalescer);
            if !out.is_empty() && tx.send(Ok(Bytes::from(out))).await.is_err() {
                return;
            }
            if done {
                terminal = true;
                break;
            }
        }

        if !terminal && upstream_done && !buffer.trim().is_empty() {
            let tail = std::mem::take(&mut buffer);
            let (out, done) = process_line(&tail, &model, &mut coalescer);
            if !out.is_empty() {
                let _ = tx.send(Ok(Bytes::from(out))).await;
            }
            terminal = done;
        }

        while !terminal && !upstream_done {
            match stream.next().await {
                Some(Ok(bytes)) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes));
                    for line in drain_lines(&mut buffer) {
                        let (out, done) = process_line(&line, &model, &mut coalescer);
                        if !out.is_empty() && tx.send(Ok(Bytes::from(out))).await.is_err() {
                            return;
                        }
                        if done {
                            terminal = true;
                            break;
                        }
                    }
                }
                Some(Err(_)) | None => {
                    if !buffer.trim().is_empty() {
                        let tail = std::mem::take(&mut buffer);
                        let (out, _) = process_line(&tail, &model, &mut coalescer);
                        if !out.is_empty() {
                            let _ = tx.send(Ok(Bytes::from(out))).await;
                        }
                    }
                    break;
                }
            }
        }

        if !terminal {
            let mut out = String::new();
            coalescer.flush(&mut out);
            if !out.is_empty() {
                let _ = tx.send(Ok(Bytes::from(out))).await;
            }
        }
    });

    let mut headers = http::HeaderMap::new();
    headers.insert(http::header::CONTENT_TYPE, http::HeaderValue::from_static("text/event-stream"));
    headers.insert(http::header::CACHE_CONTROL, http::HeaderValue::from_static("no-cache"));
    UpstreamResponse::from_stream(200, headers, Box::pin(tokio_stream_wrap(rx)))
}

fn tokio_stream_wrap(
    mut rx: tokio::sync::mpsc::Receiver<std::result::Result<Bytes, std::io::Error>>,
) -> impl futures::Stream<Item = std::result::Result<Bytes, std::io::Error>> {
    futures::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

/// Qoder executor.
pub struct QoderExecutor {
    provider: String,
    region: &'static str,
    config: &'static Json,
}

impl QoderExecutor {
    pub fn new(provider: &str) -> Self {
        let config = nr_data::provider(provider)
            .or_else(|| nr_data::provider("qoder"))
            .unwrap_or(&EMPTY_CONFIG);
        Self {
            provider: provider.to_string(),
            region: qoder_region_of(provider),
            config,
        }
    }

    fn region(&self) -> &'static str {
        self.region
    }

    /// One signed attempt. COSY signs the body, so every replay needs a fresh
    /// signature — reusing one reuses its requestId and upstream answers
    /// 403/code 103.
    async fn send_once(
        &self,
        payload: &Json,
        credentials: &Credentials,
        url: &str,
        proxy: &nr_core::http::ProxyOptions,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(UpstreamResponse, HeaderList)> {
        let plain = payload.to_string();
        let encoded = qoder_encode_body(plain.as_bytes());
        let encoded_bytes = encoded.as_bytes().to_vec();

        let cosy = cosy_headers_for(&encoded_bytes, url, credentials)
            .ok_or_else(|| Error::Provider("qoder cosy signing failed".into()))?;

        let model_key = payload
            .get("model_config")
            .and_then(|c| c.get("key"))
            .and_then(Json::as_str)
            .unwrap_or("");
        let model_source = payload
            .get("model_config")
            .and_then(|c| c.get("source"))
            .and_then(Json::as_str)
            .unwrap_or("system");

        let mut headers = HeaderList::new();
        headers.insert("Content-Type".into(), "application/json".into());
        headers.insert("Accept".into(), "text/event-stream".into());
        headers.insert("Cache-Control".into(), "no-cache".into());
        if !model_key.is_empty() {
            headers.insert("X-Model-Key".into(), model_key.to_string());
        }
        headers.insert("X-Model-Source".into(), model_source.to_string());
        // gzip triggers signature validation on Qoder's CDN; force identity.
        headers.insert("Accept-Encoding".into(), "identity".into());
        for (k, v) in cosy {
            headers.insert(k, v);
        }

        let mut req = FetchRequest::new(http::Method::POST, url);
        req.headers = headers.clone();
        req.body = Some(Bytes::from(encoded_bytes));
        req.header_timeout = Some(Duration::from_millis(
            self.config.get("timeoutMs").and_then(Json::as_u64).unwrap_or(120_000),
        ));
        req.cancel = Some(cancel.clone());

        // A failed proxy request may already have reached Qoder. Replaying the
        // same COSY signature directly reuses its requestId and returns
        // 403/code 103, so transport errors surface and the caller re-signs.
        let response = proxy_fetch(req, Some(proxy)).await?;
        Ok((response, headers))
    }

    /// Poll the model-queue status endpoint qodercli uses. `None` means the
    /// endpoint is unusable and the caller should fall back to timed replays.
    async fn fetch_queue_status(
        &self,
        credentials: &Credentials,
        proxy: &nr_core::http::ProxyOptions,
        model_key: &str,
        queue_type: Option<&str>,
        request_set_id: &str,
        request_id: &str,
        session_id: &str,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Option<self::queue::QueueInfo> {
        let base = qoder_inference_base(&credentials.clone().into_value(), self.region());
        let status_url = qoder_queue_status_url(&base, request_set_id, model_key, queue_type);

        let cosy = cosy_headers_for(&[], &status_url, credentials)?;
        let mut headers = HeaderList::new();
        headers.insert("Accept".into(), "application/json".into());
        headers.insert("Accept-Encoding".into(), "identity".into());
        // qodercli correlates the poll with the original inference.
        headers.insert("X-Request-ID".into(), request_id.to_string());
        headers.insert("X-Session-ID".into(), session_id.to_string());
        for (k, v) in cosy {
            headers.insert(k, v);
        }

        let mut req = FetchRequest::get(status_url);
        req.headers = headers;
        req.cancel = Some(cancel.clone());

        let response = proxy_fetch(req, Some(proxy)).await.ok()?;
        if !response.ok() {
            return None;
        }
        let text = response.text().await.ok()?;
        parse_qoder_queue_status(&Json::String(text))
    }
}

static EMPTY_CONFIG: std::sync::LazyLock<Json> = std::sync::LazyLock::new(|| Json::Object(Default::default()));

/// Qoder overrides `execute` entirely but still *is* a BaseExecutor upstream:
/// the registry transport config supplies `timeoutMs`/`format` and the
/// `/v1/models` path reads `baseUrls` through this hook.
impl nr_exec_base::base::BaseHooks for QoderExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn config(&self) -> &Json {
        self.config
    }
}

#[async_trait]
impl Executor for QoderExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        let ExecRequest { model, body, stream: _, credentials, cancel, log, proxy, .. } = req;
        let region = self.region();

        // PAT (pt-...) → exchange for a short-lived job token + resolve userId
        // so downstream COSY signing and catalog fetch work. Device tokens
        // (dt-...) and job tokens (jt-...) skip this and are used directly.
        let raw_token = credentials.api_key().or_else(|| credentials.access_token());
        let credentials_owned;
        let credentials: &Credentials = if is_qoder_pat(raw_token) {
            match resolve_qoder_credentials(credentials, Some(&proxy), Some(region)).await {
                Ok(c) => {
                    credentials_owned = c;
                    &credentials_owned
                }
                Err(err) => {
                    log.error("QODER", format!("PAT exchange failed: {err}"));
                    return Ok(ExecResult {
                        response: fake_json_response(401, &format!("qoder PAT exchange failed: {err}")),
                        url: build_chat_url(credentials, region),
                        headers: HeaderList::new(),
                        transformed_body: body,
                        response_format: None,
                    });
                }
            }
        } else {
            credentials
        };

        let url = build_chat_url(credentials, region);
        if credentials.psd_str("userId").is_none() {
            // No user id → no way to sign. Surface a 401 so the dashboard
            // nudges the user back to OAuth.
            return Ok(ExecResult {
                response: fake_json_response(401, "qoder credential is missing userId; reconnect the account"),
                url,
                headers: HeaderList::new(),
                transformed_body: body,
                response_format: None,
            });
        }
        if credentials.access_token().is_none() {
            // Same shape as the userId guard — a clean 401 so chatCore reports
            // "reconnect" rather than bubbling a signing error as a 500.
            return Ok(ExecResult {
                response: fake_json_response(401, "qoder credential is missing accessToken; reconnect the account"),
                url,
                headers: HeaderList::new(),
                transformed_body: body,
                response_format: None,
            });
        }

        // Attachment rewrite needs async I/O, so it runs before body building.
        let mut messages = body.get("messages").cloned().unwrap_or(Json::Array(Vec::new()));
        let uploads = self::attachments::rewrite_message_attachments(
            &mut messages,
            &mut self::attachments::RewriteCtx::new(credentials, log, Some(&proxy), region),
        )
        .await;
        if !uploads.is_empty() {
            log.debug("QODER", format!("attached {} image URL(s) to the request", uploads.len()));
        }
        let mut body_with_rewrite = body.clone();
        body_with_rewrite["messages"] = messages;

        let built = match build_request_body(model, &body_with_rewrite, credentials, log, Some(&proxy), region).await {
            Ok(b) => b,
            Err(err) => {
                log.warn("QODER", format!("request body build failed: {err}"));
                return Ok(ExecResult {
                    response: fake_json_response(400, &err.to_string()),
                    url,
                    headers: HeaderList::new(),
                    transformed_body: body,
                    response_format: None,
                });
            }
        };

        let label = format!("{}/{}", self.provider, built.qoder_key);
        let max_queue_wait = qoder_queue_max_wait_ms();
        let wait_started = Instant::now();
        let mut status_polls_enabled = true;
        let mut ready_replay_used = false;
        let mut attempt_payload = built.payload.clone();

        loop {
            let sent = match self.send_once(&attempt_payload, credentials, &url, &proxy, &cancel).await {
                Ok(sent) => sent,
                Err(err) => {
                    if cancel.is_cancelled() {
                        return Err(Error::Aborted);
                    }
                    // A transport failure may still have reached Qoder; a replay
                    // with the same signature would 403/code 103. Surface it.
                    return Err(err);
                }
            };
            let (response, headers) = sent;
            if !response.ok() {
                // Pass the error response through unchanged so chatCore can
                // capture it.
                return Ok(ExecResult {
                    response,
                    url,
                    headers,
                    transformed_body: attempt_payload,
                    response_format: None,
                });
            }

            // Peek the first frame without consuming the stream: a queued model
            // announces itself there, and a normal completion must be forwarded
            // from the already-read bytes (one upstream call per attempt).
            let mut reader = LineReader::new(response.body);
            let (peeked, _upstream_done) = reader.peek_first_frame().await;
            let peek_error = match &peeked {
                Some(EnvelopeLine::Error { status, message }) => Some((*status, message.clone())),
                _ => None,
            };

            let queue_info = peek_error.as_ref().and_then(|(_, msg)| parse_qoder_queue_info(&Json::String(msg.clone())));

            if queue_info.is_none() {
                if let Some((status_val, message)) = peek_error {
                    let status = if is_billing_block(&message) {
                        HTTP_FORBIDDEN
                    } else if (HTTP_BAD_REQUEST..=599).contains(&(status_val as u16)) {
                        status_val as u16
                    } else {
                        HTTP_BAD_GATEWAY
                    };
                    return Ok(ExecResult {
                        response: fake_json_response(
                            status,
                            &if message.is_empty() { "qoder upstream error".to_string() } else { message },
                        ),
                        url,
                        headers,
                        transformed_body: attempt_payload,
                        response_format: None,
                    });
                }
                // Clean stream. Hand the already-read bytes back instead of
                // re-sending so there is exactly one upstream call.
                let wrapped = wrap_sse(reader, &label);
                return Ok(ExecResult {
                    response: wrapped,
                    url,
                    headers,
                    transformed_body: attempt_payload,
                    response_format: None,
                });
            }

            let queue_info = queue_info.unwrap_or_default();
            let waited = wait_started.elapsed().as_millis() as u64;
            if waited >= max_queue_wait {
                log.warn(
                    "QODER",
                    format!(
                        "model queue wait timed out after {}s for {label} (queueType={}, waitTime={})",
                        waited / 1000,
                        queue_info.queue_type.as_deref().unwrap_or("?"),
                        queue_info.wait_time.map(|v| v.to_string()).unwrap_or_else(|| "?".into())
                    ),
                );
                return Ok(ExecResult {
                    response: UpstreamResponse::json(
                        503,
                        &json!({
                            "error": {
                                "message": format!(
                                    "qoder model \"{}\" is still queued after {}s (queueType={}); retry shortly",
                                    built.qoder_key,
                                    waited / 1000,
                                    queue_info.queue_type.as_deref().unwrap_or("?")
                                ),
                                "code": "qoder_model_queued",
                                "type": "rate_limit_error",
                            }
                        }),
                    ),
                    url,
                    headers,
                    transformed_body: attempt_payload,
                    response_format: None,
                });
            }

            let mut delay_ms = compute_qoder_queue_delay_ms(&queue_info, false);

            if status_polls_enabled {
                let request_set_id = attempt_payload
                    .get("request_set_id")
                    .and_then(Json::as_str)
                    .unwrap_or("")
                    .to_string();
                let request_id = attempt_payload
                    .get("request_id")
                    .and_then(Json::as_str)
                    .unwrap_or("")
                    .to_string();
                let session_id = attempt_payload
                    .get("session_id")
                    .and_then(Json::as_str)
                    .unwrap_or("")
                    .to_string();
                let polled = self
                    .fetch_queue_status(
                        credentials,
                        &proxy,
                        queue_info.model_key.as_deref().unwrap_or(&built.qoder_key),
                        queue_info.queue_type.as_deref(),
                        &request_set_id,
                        &request_id,
                        &session_id,
                        &cancel,
                    )
                    .await;

                match polled {
                    None => {
                        // No usable status endpoint — keep timed replays.
                        status_polls_enabled = false;
                    }
                    Some(info) if info.is_queued == Some(false) => {
                        // The status endpoint says ready, but it has been
                        // observed saying that while the inference call is still
                        // refused. Replay once, then stop trusting it.
                        if ready_replay_used {
                            status_polls_enabled = false;
                            log.info(
                                "QODER",
                                format!("queue status claims ready but \"{}\" is still refused; falling back to timed replays", built.qoder_key),
                            );
                        } else {
                            ready_replay_used = true;
                            log.info(
                                "QODER",
                                format!(
                                    "model \"{}\" reported ready (queueType={}); replaying",
                                    built.qoder_key,
                                    info.queue_type.as_deref().unwrap_or("?")
                                ),
                            );
                            delay_ms = 0;
                        }
                    }
                    Some(info) => {
                        delay_ms = compute_qoder_queue_delay_ms(&info, true);
                        log.info(
                            "QODER",
                            format!(
                                "model \"{}\" still queued (queueType={}, queueCount={}); rechecking in {}s",
                                built.qoder_key,
                                info.queue_type.as_deref().unwrap_or("?"),
                                info.queue_count.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
                                delay_ms / 1000
                            ),
                        );
                    }
                }
            }

            if delay_ms > 0 {
                // Sleep that aborts with the caller's cancellation, so a client
                // hanging up during a long queue wait releases the request.
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
                    _ = cancel.cancelled() => return Err(Error::Aborted),
                }
            }
        }
    }
}

/// Executor for the canonical provider id (aliases are resolved in nr-executors).
pub fn create(provider: &str) -> Arc<dyn Executor> {
    Arc::new(QoderExecutor::new(provider))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nr_exec_base::base::BaseHooks;

    fn creds() -> Credentials {
        Credentials::from_value(json!({
            "accessToken": "dt-token",
            "displayName": "Dev",
            "email": "dev@example.com",
            "providerSpecificData": { "userId": "user-1", "machineId": "machine-1" }
        }))
    }

    #[test]
    fn chat_url_uses_algo_path_and_encode_flag() {
        let url = build_chat_url(&creds(), "intl");
        assert_eq!(
            url,
            "https://api3.qoder.sh/algo/api/v2/service/pro/sse/agent_chat_generation?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1"
        );
        let cn = build_chat_url(&creds(), "cn");
        assert!(cn.starts_with("https://gateway.qoder.com.cn/algo/api/v2/service/pro/sse/agent_chat_generation"));
    }

    #[test]
    fn job_token_credentials_route_to_api2() {
        let mut c = creds();
        c.set("accessToken", Json::String("jt-abc".into()));
        assert!(build_chat_url(&c, "intl").starts_with("https://api2.qoder.sh"));
    }

    #[test]
    fn normalizes_system_out_of_messages() {
        let messages = json!([
            { "role": "system", "content": "be terse" },
            { "role": "user", "content": [{ "type": "text", "text": "hi" }] },
            { "role": "assistant", "content": "hello" }
        ]);
        let (out, system) = normalize_messages(&messages);
        assert_eq!(system, "be terse");
        assert_eq!(out.len(), 2);
        // Text-only arrays flatten to a string.
        assert_eq!(out[0]["content"], json!("hi"));
        assert_eq!(out[1]["content"], json!("hello"));
    }

    #[test]
    fn keeps_images_as_blocks() {
        let messages = json!([{
            "role": "user",
            "content": [
                { "type": "text", "text": "what is this" },
                { "type": "image_url", "image_url": { "url": "https://x/y.png" } }
            ]
        }]);
        let (out, _) = normalize_messages(&messages);
        let blocks = out[0]["content"].as_array().expect("image content stays an array");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], json!("text"));
        assert_eq!(blocks[1]["image_url"]["url"], json!("https://x/y.png"));
    }

    #[test]
    fn claude_image_blocks_become_image_url() {
        let messages = json!([{
            "role": "user",
            "content": [{
                "type": "image",
                "source": { "type": "base64", "media_type": "image/png", "data": "aGk=" }
            }]
        }]);
        let (out, _) = normalize_messages(&messages);
        let block = &out[0]["content"][0];
        assert_eq!(block["type"], json!("image_url"));
        assert_eq!(block["image_url"]["url"], json!("data:image/png;base64,aGk="));
    }

    #[test]
    fn document_blocks_become_stubs() {
        let messages = json!([{
            "role": "user",
            "content": [
                { "type": "document", "title": "spec.pdf", "source": { "type": "base64", "data": "AAA" } }
            ]
        }]);
        let (out, _) = normalize_messages(&messages);
        assert!(out[0]["content"].as_str().unwrap().contains("spec.pdf"));
        assert!(out[0]["content"].as_str().unwrap().contains("[file omitted:"));
    }

    #[test]
    fn stable_ids_are_deterministic() {
        assert_eq!(stable_hash_16("qoder-session", &["u", "auto"]), stable_hash_16("qoder-session", &["u", "auto"]));
        assert_ne!(stable_hash_16("qoder-session", &["u", "auto"]), stable_hash_16("qoder-session", &["u", "lite"]));

        let msgs = vec![json!({ "role": "user", "content": "hi" })];
        let a = stable_chat_record_id("auto", &msgs, Some(&json!([])), 100);
        let b = stable_chat_record_id("auto", &msgs, Some(&json!([])), 100);
        assert_eq!(a, b);
        let c = stable_chat_record_id("auto", &msgs, Some(&json!([])), 200);
        assert_ne!(a, c, "max tokens participate in the record id");
    }

    #[test]
    fn truncate_matches_js_slice_semantics() {
        assert_eq!(truncate_chars("short", 30), "short");
        assert_eq!(truncate_chars("abcdef", 3), "abc...");
    }

    #[test]
    fn body_payload_shape() {
        // Exercised without a catalog by using the static smodel fallback.
        let log = Logger::new();
        let body = json!({
            "messages": [
                { "role": "system", "content": "sys" },
                { "role": "user", "content": "draw a cat" }
            ],
            "max_tokens": 512,
            "tools": [{ "type": "function", "function": { "name": "f" } }]
        });
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let built = rt.block_on(build_request_body("qoder/smodel", &body, &creds(), &log, None, "intl"));
        let built = built.expect("static model config fallback");
        assert_eq!(built.qoder_key, "smodel");
        assert_eq!(built.payload["system"], json!("sys"));
        assert_eq!(built.payload["messages"][0]["content"], json!("draw a cat"));
        assert_eq!(built.payload["stream"], json!(true));
        assert_eq!(built.payload["session_type"], json!("qodercli"));
        assert_eq!(built.payload["agent_id"], json!("agent_common"));
        assert_eq!(built.payload["parameters"]["max_tokens"], json!(512));
        assert_eq!(built.payload["chat_context"]["text"], json!("draw a cat"));
        assert_eq!(built.payload["chat_context"]["extra"]["originalContent"], json!("draw a cat"));
        assert_eq!(built.payload["chat_context"]["extra"]["modelConfig"]["key"], json!("smodel"));
        assert_eq!(built.payload["chat_context"]["extra"]["modelConfig"]["is_reasoning"], json!(true));
        assert_eq!(built.payload["model_config"]["key"], json!("smodel"));
        assert_eq!(built.payload["business"]["product"], json!("cli"));
        assert_eq!(built.payload["business"]["name"], json!("draw a cat"));
        assert_eq!(built.payload["tools"][0]["function"]["name"], json!("f"));
        assert_eq!(built.payload["image_urls"], Json::Null);
        // session/record ids are stable 16-hex digests
        assert_eq!(built.payload["session_id"].as_str().unwrap().len(), 16);
        assert_eq!(built.payload["chat_record_id"].as_str().unwrap().len(), 16);
        assert_eq!(built.payload["request_set_id"], built.payload["chat_record_id"]);
    }

    #[test]
    fn unknown_model_without_catalog_is_a_hard_error() {
        let log = Logger::new();
        let body = json!({ "messages": [{ "role": "user", "content": "hi" }] });
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let err = rt
            .block_on(build_request_body("qoder/not-a-model", &body, &creds(), &log, None, "intl"))
            .err()
            .expect("must refuse an unknown model");
        assert!(err.to_string().contains("model_config for"));
    }

    #[test]
    fn max_tokens_caps_at_model_output_limit() {
        let log = Logger::new();
        let body = json!({
            "messages": [{ "role": "user", "content": "hi" }],
            "max_completion_tokens": 10
        });
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let built = rt.block_on(build_request_body("smodel", &body, &creds(), &log, None, "intl")).unwrap();
        assert_eq!(built.payload["parameters"]["max_tokens"], json!(10));
    }

    #[test]
    fn executor_hooks_match_js() {
        let ex = QoderExecutor::new("qoder-cn");
        assert_eq!(BaseHooks::provider(&ex), "qoder-cn");
        assert!(!BaseHooks::no_auth(&ex));
        assert!(!ex.base_urls().is_empty());
    }

    #[test]
    fn create_returns_requested_provider() {
        assert_eq!(create("qoder").provider(), "qoder");
        assert_eq!(create("qoder-cn").provider(), "qoder-cn");
    }
}
