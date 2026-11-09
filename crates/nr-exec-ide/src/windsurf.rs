//! Port of open-sse/executors/windsurf.js.
//!
//! WindsurfExecutor — Codeium gRPC-web chat.
//!
//! Wire protocol: gRPC-web over HTTPS (Content-Type: application/grpc-web+proto).
//! Service:  exa.language_server_pb.LanguageServerService
//! Method:   GetChatMessage (unary request → streamed CompletionChunk frames)
//!
//! Auth: credentials.accessToken = Codeium apiKey (sk-ws-... or Firebase-derived)
//!       — placed in Metadata.api_key protobuf field of every request + Bearer header.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream;
use nr_core::http::{BodyStream, HeaderList, UpstreamResponse};
use nr_core::{ExecRequest, ExecResult, Executor, Json, Result};
use serde_json::json;

const WS_BASE_URL: &str = "https://server.codeium.com";
const WS_SERVICE: &str = "exa.language_server_pb.LanguageServerService";
const WS_METHOD_CHAT: &str = "GetChatMessage";
const WS_CHAT_URL: &str = "https://server.codeium.com/exa.language_server_pb.LanguageServerService/GetChatMessage";

const WS_IDE_NAME: &str = "windsurf";
const WS_IDE_VERSION: &str = "3.14.0";
const WS_EXT_VERSION: &str = "3.14.0";
const WS_LOCALE: &str = "en-US";

pub fn resolve_ws_model_id(model: &str) -> &'static str {
    match model {
        "swe-1.6-fast" => "swe-1-6-fast",
        "swe-1.6" => "swe-1-6",
        "swe-1.5-fast" => "swe-1-5-fast",
        "swe-1.5" => "swe-1-5",
        "claude-opus-4.7-max" => "claude-opus-4-7-max",
        "claude-opus-4.7-xhigh" => "claude-opus-4-7-xhigh",
        "claude-opus-4.7-high" => "claude-opus-4-7-high",
        "claude-opus-4.7-medium" => "claude-opus-4-7-medium",
        "claude-opus-4.7-low" => "claude-opus-4-7-low",
        "claude-opus-4.7-review" => "opus-4-7-review",
        "claude-sonnet-4.6-thinking-1m" => "claude-sonnet-4-6-thinking-1m",
        "claude-sonnet-4.6-1m" => "claude-sonnet-4-6-1m",
        "claude-sonnet-4.6-thinking" => "claude-sonnet-4-6-thinking",
        "claude-sonnet-4.6" => "claude-sonnet-4-6",
        "claude-opus-4.6-thinking" => "claude-opus-4-6-thinking",
        "claude-opus-4.6" => "claude-opus-4-6",
        "claude-opus-4.5-thinking" => "MODEL_CLAUDE_4_5_OPUS_THINKING",
        "claude-opus-4.5" => "MODEL_CLAUDE_4_5_OPUS",
        "claude-sonnet-4.5-thinking" => "MODEL_PRIVATE_3",
        "claude-sonnet-4.5" => "MODEL_PRIVATE_2",
        "claude-haiku-4.5" => "MODEL_PRIVATE_11",
        "gpt-5.5-xhigh-fast" => "gpt-5-5-xhigh-priority",
        "gpt-5.5-high-fast" => "gpt-5-5-high-priority",
        "gpt-5.5-medium-fast" => "gpt-5-5-medium-priority",
        "gpt-5.5-low-fast" => "gpt-5-5-low-priority",
        "gpt-5.5-none-fast" => "gpt-5-5-none-priority",
        "gpt-5.5-xhigh" => "gpt-5-5-xhigh",
        "gpt-5.5-high" => "gpt-5-5-high",
        "gpt-5.5-medium" | "gpt-5.5" => "gpt-5-5-medium",
        "gpt-5.5-low" => "gpt-5-5-low",
        "gpt-5.5-none" => "gpt-5-5-none",
        "gpt-5.5-review" => "gpt-5-5-review",
        "gpt-5.4-xhigh-fast" => "gpt-5-4-xhigh-priority",
        "gpt-5.4-high-fast" => "gpt-5-4-high-priority",
        "gpt-5.4-medium-fast" => "gpt-5-4-medium-priority",
        "gpt-5.4-low-fast" => "gpt-5-4-low-priority",
        "gpt-5.4-none-fast" => "gpt-5-4-none-priority",
        "gpt-5.4-xhigh" => "gpt-5-4-xhigh",
        "gpt-5.4-high" => "gpt-5-4-high",
        "gpt-5.4-medium" | "gpt-5.4" => "gpt-5-4-medium",
        "gpt-5.4-low" => "gpt-5-4-low",
        "gpt-5.4-none" => "gpt-5-4-none",
        "gpt-5.4-mini-xhigh" => "gpt-5-4-mini-xhigh",
        "gpt-5.4-mini-high" => "gpt-5-4-mini-high",
        "gpt-5.4-mini-medium" => "gpt-5-4-mini-medium",
        "gpt-5.4-mini-low" => "gpt-5-4-mini-low",
        "gpt-5.3-codex-xhigh-fast" => "gpt-5-3-codex-xhigh-priority",
        "gpt-5.3-codex-high-fast" => "gpt-5-3-codex-high-priority",
        "gpt-5.3-codex-medium-fast" => "gpt-5-3-codex-medium-priority",
        "gpt-5.3-codex-low-fast" => "gpt-5-3-codex-low-priority",
        "gpt-5.3-codex-xhigh" => "gpt-5-3-codex-xhigh",
        "gpt-5.3-codex-high" => "gpt-5-3-codex-high",
        "gpt-5.3-codex-medium" | "gpt-5.3-codex" => "gpt-5-3-codex-medium",
        "gpt-5.3-codex-low" => "gpt-5-3-codex-low",
        "gpt-5.2-xhigh" => "MODEL_GPT_5_2_XHIGH",
        "gpt-5.2-high" => "MODEL_GPT_5_2_HIGH",
        "gpt-5.2-medium" | "gpt-5.2" => "MODEL_GPT_5_2_MEDIUM",
        "gpt-5.2-low" => "MODEL_GPT_5_2_LOW",
        "gpt-5.2-none" => "MODEL_GPT_5_2_NONE",
        "gpt-5" => "gpt-5",
        "gpt-4.1" => "MODEL_CHAT_GPT_4_1_2025_04_14",
        "gpt-4.1-mini" => "gpt-4.1-mini",
        "gpt-4o" => "MODEL_CHAT_GPT_4O_2024_08_06",
        "gemini-3.1-pro-high" | "gemini-3.1-pro" => "gemini-3-1-pro-high",
        "gemini-3.1-pro-low" => "gemini-3-1-pro-low",
        "gemini-3.0-flash-high" | "gemini-3.0-flash" => "MODEL_GOOGLE_GEMINI_3_0_FLASH_HIGH",
        "gemini-3.0-flash-medium" => "MODEL_GOOGLE_GEMINI_3_0_FLASH_MEDIUM",
        "gemini-3.0-flash-low" => "MODEL_GOOGLE_GEMINI_3_0_FLASH_LOW",
        "gemini-3.0-flash-minimal" => "MODEL_GOOGLE_GEMINI_3_0_FLASH_MINIMAL",
        "gemini-2.5-pro" => "MODEL_GOOGLE_GEMINI_2_5_PRO",
        "deepseek-v4" => "deepseek-v4",
        "kimi-k2.6" => "kimi-k2-6",
        "kimi-k2.5" => "kimi-k2-5",
        "glm-5.1" => "glm-5-1",
        _ => Box::leak(model.to_string().into_boxed_str()),
    }
}

// ─── Minimal Protobuf Encoder ───────────────────────────────────────────────

fn encode_varint(mut val: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    while val > 0x7f {
        bytes.push((val as u8 & 0x7f) | 0x80);
        val >>= 7;
    }
    bytes.push(val as u8 & 0x7f);
    bytes
}

fn encode_field(field_num: u32, payload: &[u8]) -> Vec<u8> {
    let tag = encode_varint(((field_num as u64) << 3) | 2);
    let len = encode_varint(payload.len() as u64);
    let mut out = Vec::with_capacity(tag.len() + len.len() + payload.len());
    out.extend_from_slice(&tag);
    out.extend_from_slice(&len);
    out.extend_from_slice(payload);
    out
}

fn encode_string(field_num: u32, s: &str) -> Vec<u8> {
    encode_field(field_num, s.as_bytes())
}

fn build_metadata(api_key: &str, session_id: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&encode_string(1, api_key));
    out.extend_from_slice(&encode_string(2, WS_IDE_NAME));
    out.extend_from_slice(&encode_string(3, WS_IDE_VERSION));
    out.extend_from_slice(&encode_string(4, WS_EXT_VERSION));
    out.extend_from_slice(&encode_string(5, session_id));
    out.extend_from_slice(&encode_string(6, WS_LOCALE));
    out
}

pub struct ChatMessage<'a> {
    pub role: &'a str,
    pub content: &'a str,
    pub tool_call_id: Option<&'a str>,
}

fn build_chat_message(msg: &ChatMessage) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&encode_string(1, msg.role));
    out.extend_from_slice(&encode_string(2, msg.content));
    if let Some(tid) = msg.tool_call_id {
        out.extend_from_slice(&encode_string(3, tid));
    }
    out
}

pub fn build_get_chat_message_request(api_key: &str, model: &str, messages: &[ChatMessage]) -> Vec<u8> {
    let session_id = uuid::Uuid::new_v4().to_string();
    let cascade_id = uuid::Uuid::new_v4().to_string();

    let mut out = Vec::new();
    out.extend_from_slice(&encode_field(1, &build_metadata(api_key, &session_id)));
    out.extend_from_slice(&encode_string(2, &cascade_id));
    out.extend_from_slice(&encode_field(3, &encode_string(1, model)));

    for msg in messages {
        out.extend_from_slice(&encode_field(4, &build_chat_message(msg)));
    }

    out
}

pub fn grpc_web_frame(payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u32;
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(0x00); // no compression
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

// ─── Protobuf Decoder ───────────────────────────────────────────────────────

fn read_varint(buf: &[u8], mut offset: usize) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0;
    while offset < buf.len() {
        let b = buf[offset];
        offset += 1;
        result |= ((b & 0x7f) as u64) << shift;
        if (b & 0x80) == 0 {
            return Some((result, offset));
        }
        shift += 7;
    }
    None
}

fn decode_string_field(buf: &[u8], target_field: u32) -> Option<String> {
    let mut offset = 0;
    while offset < buf.len() {
        let (tag, new_off) = read_varint(buf, offset)?;
        offset = new_off;
        let field_num = (tag >> 3) as u32;
        let wire_type = tag & 0x07;
        if wire_type == 2 {
            let (len, new_off) = read_varint(buf, offset)?;
            offset = new_off;
            let len = len as usize;
            if offset + len > buf.len() {
                break;
            }
            let payload = &buf[offset..offset + len];
            offset += len;
            if field_num == target_field {
                return String::from_utf8(payload.to_vec()).ok();
            }
        } else if wire_type == 0 {
            let (_, new_off) = read_varint(buf, offset)?;
            offset = new_off;
        } else if wire_type == 1 {
            offset += 8;
        } else if wire_type == 5 {
            offset += 4;
        } else {
            break;
        }
    }
    None
}

fn decode_done_chunk(buf: &[u8]) -> (i64, i64) {
    let mut offset = 0;
    let mut usage_bytes = None;
    while offset < buf.len() {
        let Some((tag, new_off)) = read_varint(buf, offset) else { break };
        offset = new_off;
        let field_num = (tag >> 3) as u32;
        let wire_type = tag & 0x07;
        if wire_type == 2 {
            let Some((len, new_off)) = read_varint(buf, offset) else { break };
            offset = new_off;
            let len = len as usize;
            if offset + len > buf.len() { break; }
            if field_num == 1 {
                usage_bytes = Some(&buf[offset..offset + len]);
            }
            offset += len;
        } else if wire_type == 0 {
            let Some((_, new_off)) = read_varint(buf, offset) else { break };
            offset = new_off;
        } else {
            break;
        }
    }

    let Some(ub) = usage_bytes else { return (0, 0) };
    let mut prompt_tokens = 0;
    let mut completion_tokens = 0;
    offset = 0;
    while offset < ub.len() {
        let Some((tag, new_off)) = read_varint(ub, offset) else { break };
        offset = new_off;
        let field_num = (tag >> 3) as u32;
        let wire_type = tag & 0x07;
        if wire_type == 0 {
            let Some((v, new_off)) = read_varint(ub, offset) else { break };
            offset = new_off;
            if field_num == 1 { prompt_tokens = v as i64; }
            else if field_num == 2 { completion_tokens = v as i64; }
        } else if wire_type == 2 {
            let Some((len, new_off)) = read_varint(ub, offset) else { break };
            offset = new_off + len as usize;
        } else {
            break;
        }
    }
    (prompt_tokens, completion_tokens)
}

#[derive(Debug, PartialEq, Eq)]
pub enum CompletionChunk {
    Content(String),
    Done { prompt_tokens: i64, completion_tokens: i64 },
    Error(String),
    Unknown,
}

pub fn decode_completion_chunk(buf: &[u8]) -> CompletionChunk {
    let mut offset = 0;
    while offset < buf.len() {
        let Some((tag, new_off)) = read_varint(buf, offset) else { break };
        offset = new_off;
        let field_num = (tag >> 3) as u32;
        let wire_type = tag & 0x07;

        if wire_type == 2 {
            let Some((len, new_off)) = read_varint(buf, offset) else { break };
            offset = new_off;
            let len = len as usize;
            if offset + len > buf.len() { break; }
            let payload = &buf[offset..offset + len];
            offset += len;

            if field_num == 1 {
                if let Some(text) = decode_string_field(payload, 1) {
                    return CompletionChunk::Content(text);
                }
            } else if field_num == 3 {
                let (pt, ct) = decode_done_chunk(payload);
                return CompletionChunk::Done { prompt_tokens: pt, completion_tokens: ct };
            } else if field_num == 4 {
                let msg = decode_string_field(payload, 1).unwrap_or_else(|| "unknown windsurf error".to_string());
                return CompletionChunk::Error(msg);
            }
        } else if wire_type == 0 {
            let Some((_, new_off)) = read_varint(buf, offset) else { break };
            offset = new_off;
        } else if wire_type == 1 {
            offset += 8;
        } else if wire_type == 5 {
            offset += 4;
        } else {
            break;
        }
    }
    CompletionChunk::Unknown
}

pub struct WindsurfExecutor {
    provider: String,
    config: &'static nr_core::Json,
}

impl WindsurfExecutor {
    pub fn new(provider: &str) -> Self {
        let config = nr_data::provider(provider)
            .or_else(|| nr_data::provider("windsurf"))
            .unwrap_or(&EMPTY_CONFIG);
        Self {
            provider: provider.to_string(),
            config,
        }
    }
}

impl nr_exec_base::base::BaseHooks for WindsurfExecutor {
    fn provider(&self) -> &str {
        "windsurf"
    }

    fn config(&self) -> &nr_core::Json {
        self.config
    }
}

#[async_trait]
impl Executor for WindsurfExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn no_auth(&self) -> bool {
        false
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        let api_key = req.credentials.access_token()
            .or_else(|| req.credentials.api_key())
            .unwrap_or("");

        let ws_model = resolve_ws_model_id(req.model);
        let raw_messages = req.body.get("messages").and_then(Json::as_array).cloned().unwrap_or_default();

        let mut chat_messages = Vec::new();
        for m in &raw_messages {
            let role = m.get("role").and_then(Json::as_str).unwrap_or("user");
            let content = m.get("content").and_then(Json::as_str).unwrap_or("");
            let tid = m.get("tool_call_id").and_then(Json::as_str);
            chat_messages.push(ChatMessage { role, content, tool_call_id: tid });
        }
        if chat_messages.is_empty() {
            chat_messages.push(ChatMessage { role: "user", content: "", tool_call_id: None });
        }

        let proto_payload = build_get_chat_message_request(api_key, ws_model, &chat_messages);
        let framed_payload = grpc_web_frame(&proto_payload);

        let mut req_headers = reqwest::header::HeaderMap::new();
        req_headers.insert("Content-Type", reqwest::header::HeaderValue::from_static("application/grpc-web+proto"));
        req_headers.insert("Accept", reqwest::header::HeaderValue::from_static("application/grpc-web+proto"));
        req_headers.insert("User-Agent", reqwest::header::HeaderValue::from_str(&format!("windsurf/{WS_IDE_VERSION}")).unwrap_or(reqwest::header::HeaderValue::from_static("windsurf")));
        req_headers.insert("X-Grpc-Web", reqwest::header::HeaderValue::from_static("1"));

        if !api_key.is_empty() {
            if let Ok(val) = reqwest::header::HeaderValue::from_str(&format!("Bearer {api_key}")) {
                req_headers.insert(reqwest::header::AUTHORIZATION, val);
            }
        }

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(300000))
            .build()
            .unwrap_or_default();

        let res = match client.post(WS_CHAT_URL).headers(req_headers).body(framed_payload).send().await {
            Ok(r) => r,
            Err(e) => {
                let err = json!({
                    "error": { "message": format!("Windsurf request failed: {e}"), "type": "upstream_error" }
                });
                return Ok(ExecResult {
                    response: UpstreamResponse::json(502, &err),
                    url: WS_CHAT_URL.to_string(),
                    headers: HeaderList::new(),
                    transformed_body: req.body,
                    response_format: None,
                });
            }
        };

        if !res.status().is_success() {
            let status = res.status().as_u16();
            let text = res.text().await.unwrap_or_default();
            let err = json!({
                "error": { "message": format!("Windsurf HTTP {status}: {text}"), "type": "upstream_error" }
            });
            return Ok(ExecResult {
                response: UpstreamResponse::json(status, &err),
                url: WS_CHAT_URL.to_string(),
                headers: HeaderList::new(),
                transformed_body: req.body,
                response_format: None,
            });
        }

        let raw_bytes = res.bytes().await.unwrap_or_default();
        let response_id = format!("chatcmpl-ws-{}", chrono::Utc::now().timestamp_millis());
        let created = chrono::Utc::now().timestamp();
        let model = req.model.to_string();

        let mut offset = 0;
        let mut total_text = String::new();
        let mut prompt_tokens = 0;
        let mut completion_tokens = 0;
        let mut had_error = None;
        let mut sse_chunks = String::new();

        if req.stream {
            let init_chunk = json!({
                "id": response_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{ "index": 0, "delta": { "role": "assistant" }, "finish_reason": null }]
            });
            sse_chunks.push_str(&format!("data: {init_chunk}\n\n"));
        }

        while offset + 5 <= raw_bytes.len() {
            let flag = raw_bytes[offset];
            let len = u32::from_be_bytes([
                raw_bytes[offset + 1],
                raw_bytes[offset + 2],
                raw_bytes[offset + 3],
                raw_bytes[offset + 4],
            ]) as usize;

            if offset + 5 + len > raw_bytes.len() {
                break;
            }
            let payload = &raw_bytes[offset + 5..offset + 5 + len];
            offset += 5 + len;

            if flag == 0x80 {
                // Trailer frame: check grpc-status
                let trailer_str = String::from_utf8_lossy(payload);
                for line in trailer_str.lines() {
                    if let Some(status_str) = line.strip_prefix("grpc-status:") {
                        let code = status_str.trim();
                        if code != "0" {
                            had_error = Some(format!("gRPC status {code}"));
                        }
                    }
                }
                continue;
            }

            if flag != 0x00 {
                continue;
            }

            match decode_completion_chunk(payload) {
                CompletionChunk::Content(text) => {
                    total_text.push_str(&text);
                    if req.stream {
                        let delta_chunk = json!({
                            "id": response_id,
                            "object": "chat.completion.chunk",
                            "created": created,
                            "model": model,
                            "choices": [{ "index": 0, "delta": { "content": text }, "finish_reason": null }]
                        });
                        sse_chunks.push_str(&format!("data: {delta_chunk}\n\n"));
                    }
                }
                CompletionChunk::Done { prompt_tokens: pt, completion_tokens: ct } => {
                    prompt_tokens = pt;
                    completion_tokens = ct;
                }
                CompletionChunk::Error(msg) => {
                    had_error = Some(msg);
                }
                CompletionChunk::Unknown => {}
            }
        }

        if let Some(err) = had_error {
            let err_json = json!({
                "error": { "message": err, "type": "windsurf_error" }
            });
            return Ok(ExecResult {
                response: UpstreamResponse::json(502, &err_json),
                url: WS_CHAT_URL.to_string(),
                headers: HeaderList::new(),
                transformed_body: req.body,
                response_format: None,
            });
        }

        if req.stream {
            let stop_chunk = json!({
                "id": response_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
                "usage": {
                    "prompt_tokens": prompt_tokens,
                    "completion_tokens": completion_tokens,
                    "total_tokens": prompt_tokens + completion_tokens
                }
            });
            sse_chunks.push_str(&format!("data: {stop_chunk}\n\n"));
            sse_chunks.push_str("data: [DONE]\n\n");

            let bytes = Bytes::from(sse_chunks);
            let body_stream: BodyStream = Box::pin(stream::once(async move { Ok(bytes) }));

            return Ok(ExecResult {
                response: UpstreamResponse::sse(body_stream),
                url: WS_CHAT_URL.to_string(),
                headers: HeaderList::new(),
                transformed_body: req.body,
                response_format: None,
            });
        }

        // Non-streaming response
        let completion = json!({
            "id": response_id,
            "object": "chat.completion",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": total_text },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": prompt_tokens + completion_tokens
            }
        });

        Ok(ExecResult {
            response: UpstreamResponse::json(200, &completion),
            url: WS_CHAT_URL.to_string(),
            headers: HeaderList::new(),
            transformed_body: req.body,
            response_format: None,
        })
    }
}

pub fn create(provider: &str) -> Arc<dyn Executor> {
    Arc::new(WindsurfExecutor::new(provider))
}

static EMPTY_CONFIG: std::sync::LazyLock<nr_core::Json> =
    std::sync::LazyLock::new(|| nr_core::Json::Object(Default::default()));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windsurf_executor_creates() {
        let executor = WindsurfExecutor::new("windsurf");
        assert_eq!(executor.provider(), "windsurf");
    }

    #[test]
    fn test_resolve_ws_model_id() {
        assert_eq!(resolve_ws_model_id("gpt-5.5"), "gpt-5-5-medium");
        assert_eq!(resolve_ws_model_id("claude-sonnet-4.6"), "claude-sonnet-4-6");
        assert_eq!(resolve_ws_model_id("swe-1.6"), "swe-1-6");
    }

    #[test]
    fn test_varint_encoding() {
        assert_eq!(encode_varint(1), vec![1]);
        assert_eq!(encode_varint(300), vec![0xac, 0x02]);
    }

    #[test]
    fn test_grpc_web_frame() {
        let payload = b"hello";
        let frame = grpc_web_frame(payload);
        assert_eq!(frame.len(), 10);
        assert_eq!(frame[0], 0x00);
        assert_eq!(&frame[1..5], &5u32.to_be_bytes());
        assert_eq!(&frame[5..], payload);
    }
}
