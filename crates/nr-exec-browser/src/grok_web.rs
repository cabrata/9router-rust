//! Port of open-sse/executors/grok-web.js + grok-web-tools.js
//! Grok web chat via XAI API + native tool bridge
//! Auth: Grok cookies (sso, __cf_bm, cf_clearance)
//! Response: NDJSON stream with tool call parsing

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use bytes::Bytes;
use futures::{stream, StreamExt};
use http::HeaderMap;
use nr_core::http::{BodyStream, HeaderList, UpstreamResponse};
use nr_core::{ExecRequest, ExecResult, Executor, Json, Result};
use rand::Rng;
use regex::Regex;
use serde_json::json;

const GROK_CHAT_API: &str = "https://grok.com/rest/app-chat/conversations/new";
const GROK_TLS_USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36";

struct ModelInfo {
    grok_model: &'static str,
    model_mode: &'static str,
    is_thinking: bool,
}

fn resolve_model_info(model: &str) -> ModelInfo {
    match model {
        "grok-3" => ModelInfo { grok_model: "grok-4-1-thinking-1129", model_mode: "MODEL_MODE_FAST", is_thinking: false },
        "grok-3-mini" => ModelInfo { grok_model: "grok-4-1-thinking-1129", model_mode: "MODEL_MODE_GROK_4_1_MINI_THINKING", is_thinking: true },
        "grok-3-thinking" => ModelInfo { grok_model: "grok-4-1-thinking-1129", model_mode: "MODEL_MODE_GROK_4_1_THINKING", is_thinking: true },
        "grok-4" => ModelInfo { grok_model: "grok-4-1-thinking-1129", model_mode: "MODEL_MODE_FAST", is_thinking: false },
        "grok-4-mini" => ModelInfo { grok_model: "grok-4-1-thinking-1129", model_mode: "MODEL_MODE_GROK_4_1_MINI_THINKING", is_thinking: true },
        "grok-4-thinking" => ModelInfo { grok_model: "grok-4-1-thinking-1129", model_mode: "MODEL_MODE_GROK_4_1_THINKING", is_thinking: true },
        "grok-4-heavy" => ModelInfo { grok_model: "grok-4-1-thinking-1129", model_mode: "MODEL_MODE_EXPERT", is_thinking: true },
        "grok-4.1-mini" => ModelInfo { grok_model: "grok-4-1-thinking-1129", model_mode: "MODEL_MODE_GROK_4_1_MINI_THINKING", is_thinking: true },
        "grok-4.1-fast" => ModelInfo { grok_model: "grok-4-1-thinking-1129", model_mode: "MODEL_MODE_FAST", is_thinking: false },
        "grok-4.1-expert" => ModelInfo { grok_model: "grok-4-1-thinking-1129", model_mode: "MODEL_MODE_EXPERT", is_thinking: true },
        "grok-4.1-thinking" => ModelInfo { grok_model: "grok-4-1-thinking-1129", model_mode: "MODEL_MODE_GROK_4_1_THINKING", is_thinking: true },
        "grok-4.2" | "grok-4.20" | "grok-4.20-beta" => ModelInfo { grok_model: "grok-4-1-thinking-1129", model_mode: "MODEL_MODE_GROK_420", is_thinking: false },
        _ => ModelInfo { grok_model: "grok-4-1-thinking-1129", model_mode: "MODEL_MODE_FAST", is_thinking: false },
    }
}

pub fn random_hex(bytes: usize) -> String {
    let mut rng = rand::thread_rng();
    let mut buf = vec![0u8; bytes];
    rng.fill(&mut buf[..]);
    hex::encode(buf)
}

fn random_string(len: usize, alphanumeric: bool) -> String {
    let mut rng = rand::thread_rng();
    let chars: &[u8] = if alphanumeric {
        b"abcdefghijklmnopqrstuvwxyz0123456789"
    } else {
        b"abcdefghijklmnopqrstuvwxyz"
    };
    (0..len)
        .map(|_| {
            let idx = rng.gen_range(0..chars.len());
            chars[idx] as char
        })
        .collect()
}

pub fn generate_statsig_id() -> String {
    let mut rng = rand::thread_rng();
    let msg = if rng.gen_bool(0.5) {
        format!(
            "x1:TypeError: Cannot read properties of null (reading 'children[\"{}\"]')",
            random_string(5, true)
        )
    } else {
        format!(
            "x1:TypeError: Cannot read properties of undefined (reading '{}')",
            random_string(10, false)
        )
    };
    base64::engine::general_purpose::STANDARD.encode(msg.as_bytes())
}

pub fn build_grok_cookie_header(credential: &str) -> String {
    let trimmed = credential.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    // If it's already a Cookie header format or raw sso token
    if trimmed.contains('=') {
        trimmed.to_string()
    } else {
        format!("sso={trimmed}")
    }
}

pub fn parse_openai_messages(messages: &[Json]) -> String {
    let mut extracted = Vec::new();
    for msg in messages {
        let role = msg.get("role").and_then(Json::as_str).unwrap_or("user");
        let mapped_role = if role == "developer" { "system" } else { role };

        let mut content = String::new();
        if let Some(s) = msg.get("content").and_then(Json::as_str) {
            content.push_str(s);
        } else if let Some(arr) = msg.get("content").and_then(Json::as_array) {
            for part in arr {
                if part.get("type").and_then(Json::as_str) == Some("text") {
                    if let Some(t) = part.get("text").and_then(Json::as_str) {
                        content.push_str(t);
                    }
                }
            }
        }
        if !content.trim().is_empty() {
            extracted.push((mapped_role.to_string(), content));
        }
    }

    let last_user_idx = extracted.iter().rposition(|(r, _)| r == "user");
    let mut parts = Vec::new();
    for (i, (role, text)) in extracted.iter().enumerate() {
        if Some(i) == last_user_idx {
            parts.push(text.clone());
        } else {
            parts.push(format!("{role}: {text}"));
        }
    }
    parts.join("\n\n")
}

pub fn extract_tool_calls_from_markup(text: &str) -> (String, Vec<Json>) {
    let re = match Regex::new(r"(?s)<tool_call>(.*?)</tool_call>") {
        Ok(r) => r,
        Err(_) => return (text.to_string(), Vec::new()),
    };

    let mut calls = Vec::new();
    let mut cleaned = text.to_string();

    for (i, cap) in re.captures_iter(text).enumerate() {
        if let Some(block) = cap.get(1) {
            let inner = block.as_str().trim();
            if let Ok(parsed) = serde_json::from_str::<Json>(inner) {
                let name = parsed.get("name").and_then(Json::as_str).unwrap_or("tool");
                let args = parsed.get("arguments").cloned().unwrap_or(json!({}));
                let args_str = if args.is_string() {
                    args.as_str().unwrap().to_string()
                } else {
                    args.to_string()
                };
                calls.push(json!({
                    "index": i,
                    "id": format!("call_{}", uuid::Uuid::new_v4().simple()),
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": args_str,
                    }
                }));
            }
        }
    }

    if !calls.is_empty() {
        cleaned = re.replace_all(&cleaned, "").trim().to_string();
    }

    (cleaned, calls)
}

/// GrokWebExecutor — Grok web chat via web endpoint
pub struct GrokWebExecutor {
    pub provider: String,
}

impl GrokWebExecutor {
    pub fn new() -> Self {
        Self {
            provider: "grok-web".to_string(),
        }
    }
}

impl Default for GrokWebExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Executor for GrokWebExecutor {
    fn provider(&self) -> &str {
        "grok-web"
    }

    fn no_auth(&self) -> bool {
        false
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        let messages = req.body.get("messages").and_then(Json::as_array).cloned().unwrap_or_default();
        if messages.is_empty() {
            let err = json!({
                "error": { "message": "Missing or empty messages array", "type": "invalid_request" }
            });
            return Ok(ExecResult {
                response: UpstreamResponse::json(400, &err),
                url: GROK_CHAT_API.to_string(),
                headers: HeaderList::new(),
                transformed_body: req.body,
                response_format: None,
            });
        }

        let model_info = resolve_model_info(req.model);
        let prompt_text = parse_openai_messages(&messages);
        if prompt_text.trim().is_empty() {
            let err = json!({
                "error": { "message": "Empty query after processing", "type": "invalid_request" }
            });
            return Ok(ExecResult {
                response: UpstreamResponse::json(400, &err),
                url: GROK_CHAT_API.to_string(),
                headers: HeaderList::new(),
                transformed_body: req.body,
                response_format: None,
            });
        }

        let cookie_header = req
            .credentials
            .api_key()
            .map(build_grok_cookie_header)
            .unwrap_or_default();

        let trace_id = random_hex(16);
        let span_id = random_hex(8);
        let browser_id = uuid::Uuid::new_v4().to_string();
        let request_id = uuid::Uuid::new_v4().to_string();
        let statsig_id = generate_statsig_id();

        let grok_payload = json!({
            "temporary": true,
            "modelName": model_info.grok_model,
            "modelMode": model_info.model_mode,
            "message": prompt_text,
            "fileAttachments": [],
            "imageAttachments": [],
            "disableSearch": false,
            "enableImageGeneration": false,
            "returnImageBytes": false,
            "returnRawGrokInXaiRequest": false,
            "enableImageStreaming": false,
            "imageGenerationCount": 0,
            "forceConcise": false,
            "toolOverrides": {},
            "enableSideBySide": true,
            "sendFinalMetadata": true,
            "isReasoning": false,
            "disableTextFollowUps": false,
            "disableMemory": true,
            "forceSideBySide": false,
            "isAsyncChat": false,
            "disableSelfHarmShortCircuit": false,
            "deviceEnvInfo": {
                "darkModeEnabled": false,
                "devicePixelRatio": 2,
                "screenWidth": 2056,
                "screenHeight": 1329,
                "viewportWidth": 2056,
                "viewportHeight": 1083
            }
        });

        let mut req_headers = reqwest::header::HeaderMap::new();
        req_headers.insert("Accept", reqwest::header::HeaderValue::from_static("*/*"));
        req_headers.insert("Content-Type", reqwest::header::HeaderValue::from_static("application/json"));
        req_headers.insert("Origin", reqwest::header::HeaderValue::from_static("https://grok.com"));
        req_headers.insert("Referer", reqwest::header::HeaderValue::from_static("https://grok.com/"));
        req_headers.insert("User-Agent", reqwest::header::HeaderValue::from_static(GROK_TLS_USER_AGENT));
        req_headers.insert("x-statsig-id", reqwest::header::HeaderValue::from_str(&statsig_id).unwrap_or(reqwest::header::HeaderValue::from_static("")));
        req_headers.insert("Browser-Id", reqwest::header::HeaderValue::from_str(&browser_id).unwrap_or(reqwest::header::HeaderValue::from_static("")));
        req_headers.insert("x-xai-request-id", reqwest::header::HeaderValue::from_str(&request_id).unwrap_or(reqwest::header::HeaderValue::from_static("")));
        req_headers.insert("traceparent", reqwest::header::HeaderValue::from_str(&format!("00-{trace_id}-{span_id}-00")).unwrap_or(reqwest::header::HeaderValue::from_static("")));

        if !cookie_header.is_empty() {
            if let Ok(val) = reqwest::header::HeaderValue::from_str(&cookie_header) {
                req_headers.insert(reqwest::header::COOKIE, val);
            }
        }

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(120000))
            .build()
            .unwrap_or_default();

        let response = match client
            .post(GROK_CHAT_API)
            .headers(req_headers)
            .json(&grok_payload)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let err = json!({
                    "error": { "message": format!("Grok connection failed: {e}"), "type": "upstream_error" }
                });
                return Ok(ExecResult {
                    response: UpstreamResponse::json(502, &err),
                    url: GROK_CHAT_API.to_string(),
                    headers: HeaderList::new(),
                    transformed_body: grok_payload,
                    response_format: None,
                });
            }
        };

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body_text = response.text().await.unwrap_or_default();
            let msg = if body_text.to_lowercase().contains("cloudflare") || body_text.to_lowercase().contains("turnstile") {
                "Grok returned a Cloudflare challenge — cf_clearance in the pasted Cookie header no longer matches this machine. Re-copy the full Cookie header from a logged-in grok.com browser on this machine.".to_string()
            } else if status == 401 || status == 403 {
                "Grok auth failed — SSO cookie may be expired. Re-paste your sso cookie value from grok.com.".to_string()
            } else if status == 429 {
                "Grok rate limited. Wait a moment and retry, or rotate cookies.".to_string()
            } else {
                format!("Grok returned HTTP {status}")
            };

            let err = json!({
                "error": { "message": msg, "type": "upstream_error", "code": format!("HTTP_{status}") }
            });
            return Ok(ExecResult {
                response: UpstreamResponse::json(status, &err),
                url: GROK_CHAT_API.to_string(),
                headers: HeaderList::new(),
                transformed_body: grok_payload,
                response_format: None,
            });
        }

        let body_text = response.text().await.unwrap_or_default();

        let cid = format!("chatcmpl-grok-{}", &uuid::Uuid::new_v4().to_string()[..12]);
        let created = chrono::Utc::now().timestamp();
        let model = req.model.to_string();

        if req.stream {
            let mut sse_output = String::new();

            // Initial role chunk
            let init_chunk = json!({
                "id": cid,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": { "role": "assistant" },
                    "finish_reason": null
                }]
            });
            sse_output.push_str(&format!("data: {init_chunk}\n\n"));

            let mut full_text = String::new();
            let mut thinking_text = String::new();

            for line in body_text.lines() {
                let trimmed = line.trim();
                if !trimmed.starts_with('{') {
                    continue;
                }
                let Ok(event) = serde_json::from_str::<Json>(trimmed) else {
                    continue;
                };

                if let Some(err_msg) = event.get("error").and_then(|e| e.get("message")).and_then(Json::as_str) {
                    let err_chunk = json!({
                        "id": cid,
                        "object": "chat.completion.chunk",
                        "created": created,
                        "model": model,
                        "choices": [{
                            "index": 0,
                            "delta": { "content": format!("[Error: {err_msg}]") },
                            "finish_reason": null
                        }]
                    });
                    sse_output.push_str(&format!("data: {err_chunk}\n\n"));
                    break;
                }

                let resp = event.get("result").and_then(|r| r.get("response"));
                if let Some(resp) = resp {
                    if let Some(token) = resp.get("token").and_then(Json::as_str) {
                        full_text.push_str(token);
                        let chunk = json!({
                            "id": cid,
                            "object": "chat.completion.chunk",
                            "created": created,
                            "model": model,
                            "choices": [{
                                "index": 0,
                                "delta": { "content": token },
                                "finish_reason": null
                            }]
                        });
                        sse_output.push_str(&format!("data: {chunk}\n\n"));
                    } else if let Some(mr) = resp.get("modelResponse") {
                        if let Some(msg) = mr.get("message").and_then(Json::as_str) {
                            if model_info.is_thinking && thinking_text.is_empty() {
                                thinking_text.push_str(msg);
                                let think_chunk = json!({
                                    "id": cid,
                                    "object": "chat.completion.chunk",
                                    "created": created,
                                    "model": model,
                                    "choices": [{
                                        "index": 0,
                                        "delta": { "reasoning_content": msg },
                                        "finish_reason": null
                                    }]
                                });
                                sse_output.push_str(&format!("data: {think_chunk}\n\n"));
                            }
                        }
                    }
                }
            }

            // Check for tool calls in complete output
            let (_, tool_calls) = extract_tool_calls_from_markup(&full_text);
            if !tool_calls.is_empty() {
                for (i, tc) in tool_calls.iter().enumerate() {
                    let tc_chunk = json!({
                        "id": cid,
                        "object": "chat.completion.chunk",
                        "created": created,
                        "model": model,
                        "choices": [{
                            "index": 0,
                            "delta": { "tool_calls": [tc] },
                            "finish_reason": null
                        }]
                    });
                    sse_output.push_str(&format!("data: {tc_chunk}\n\n"));
                }
                let stop_chunk = json!({
                    "id": cid,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }]
                });
                sse_output.push_str(&format!("data: {stop_chunk}\n\n"));
            } else {
                let stop_chunk = json!({
                    "id": cid,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }]
                });
                sse_output.push_str(&format!("data: {stop_chunk}\n\n"));
            }

            sse_output.push_str("data: [DONE]\n\n");

            let stream_bytes = Bytes::from(sse_output);
            let body_stream: BodyStream = Box::pin(stream::once(async move { Ok(stream_bytes) }));
            let response = UpstreamResponse::sse(body_stream);

            return Ok(ExecResult {
                response,
                url: GROK_CHAT_API.to_string(),
                headers: HeaderList::new(),
                transformed_body: grok_payload,
                response_format: None,
            });
        }

        // Non-streaming response collection
        let mut full_text = String::new();
        let mut thinking_text = String::new();

        for line in body_text.lines() {
            let trimmed = line.trim();
            if !trimmed.starts_with('{') {
                continue;
            }
            let Ok(event) = serde_json::from_str::<Json>(trimmed) else {
                continue;
            };

            let resp = event.get("result").and_then(|r| r.get("response"));
            if let Some(resp) = resp {
                if let Some(msg) = resp.get("modelResponse").and_then(|mr| mr.get("message")).and_then(Json::as_str) {
                    full_text = msg.to_string();
                } else if let Some(token) = resp.get("token").and_then(Json::as_str) {
                    full_text.push_str(token);
                }
            }
        }

        let (cleaned_text, tool_calls) = extract_tool_calls_from_markup(&full_text);
        let finish_reason = if !tool_calls.is_empty() { "tool_calls" } else { "stop" };

        let mut msg_obj = json!({
            "role": "assistant",
            "content": cleaned_text
        });
        if !thinking_text.is_empty() {
            msg_obj["reasoning_content"] = Json::String(thinking_text);
        }
        if !tool_calls.is_empty() {
            msg_obj["tool_calls"] = Json::Array(tool_calls);
        }

        let prompt_tokens = (prompt_text.len() / 4).max(1);
        let completion_tokens = (full_text.len() / 4).max(1);

        let completion = json!({
            "id": cid,
            "object": "chat.completion",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "message": msg_obj,
                "finish_reason": finish_reason,
                "logprobs": null
            }],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": prompt_tokens + completion_tokens
            }
        });

        Ok(ExecResult {
            response: UpstreamResponse::json(200, &completion),
            url: GROK_CHAT_API.to_string(),
            headers: HeaderList::new(),
            transformed_body: grok_payload,
            response_format: None,
        })
    }
}

/// Create executor for the canonical provider id
pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(GrokWebExecutor::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_grok_cookie_header() {
        let cred = "sso=abc123; __cf_bm=def456";
        let header = build_grok_cookie_header(cred);
        assert!(header.contains("sso="));

        let bare = "abc123token";
        let header_bare = build_grok_cookie_header(bare);
        assert_eq!(header_bare, "sso=abc123token");
    }

    #[test]
    fn test_resolve_model_info() {
        let info = resolve_model_info("grok-3");
        assert_eq!(info.grok_model, "grok-4-1-thinking-1129");
        assert_eq!(info.model_mode, "MODEL_MODE_FAST");
        assert!(!info.is_thinking);

        let thinking = resolve_model_info("grok-3-thinking");
        assert!(thinking.is_thinking);
    }

    #[test]
    fn test_parse_openai_messages() {
        let msgs = json!([
            {"role": "system", "content": "You are helpful"},
            {"role": "user", "content": "Hello"}
        ]);
        let parsed = parse_openai_messages(msgs.as_array().unwrap());
        assert!(parsed.contains("system: You are helpful"));
        assert!(parsed.contains("Hello"));
    }

    #[test]
    fn test_extract_tool_calls_from_markup() {
        let text = "Here is a call:\n<tool_call>{\"name\":\"weather\",\"arguments\":{\"city\":\"Paris\"}}</tool_call>\nDone.";
        let (cleaned, calls) = extract_tool_calls_from_markup(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "weather");
        assert!(!cleaned.contains("<tool_call>"));
    }

    #[test]
    fn test_random_hex_length() {
        let hex16 = random_hex(16);
        assert_eq!(hex16.len(), 32);
    }
}
