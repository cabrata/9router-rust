//! Port of open-sse/executors/chatgpt-web.js — ChatGPT web chat executor.
//!
//! Handles session-based authentication (session token / cookie), Sentinel PoW/prepare headers,
//! plain HTTP turn execution (`/f/conversation/prepare` → `/f/conversation` SSE → conversation polling),
//! and tool calling via `<tool>` blocks.

use std::sync::Arc;
use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;
use nr_common::sse::sse_chunk;
use nr_core::http::{HeaderList, UpstreamResponse};
use nr_core::{Credentials, Error, ExecRequest, ExecResult, Executor, Json, Result};
use serde_json::json;
use uuid::Uuid;

use crate::chat_http::run_chat_gpt_web_http_turn;
use crate::web_tools::{build_tool_aware_result, prepare_tool_messages, ToolCall};

const CHATGPT_WEB_URL: &str = "https://chatgpt.com";
const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36";

/// ChatGPT web executor — handles chatgpt.com web API
pub struct ChatGptWebExecutor;

// shortcut: Clean-room browser context execution requires headed Chromium / Playwright; plain-HTTP execution path handles standard chat and tool turns
#[async_trait]
impl Executor for ChatGptWebExecutor {
    fn provider(&self) -> &str {
        "chatgpt-web"
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        let cookie = extract_cookie(req.credentials)?;
        let user_agent = req.credentials
            .psd_str("customUserAgent")
            .unwrap_or(DEFAULT_USER_AGENT)
            .to_string();

        let access_token = match req.credentials.access_token() {
            Some(tok) if !tok.is_empty() => tok.to_string(),
            _ => fetch_access_token(&cookie, &user_agent).await?,
        };

        let messages = req.body.get("messages").and_then(|m| m.as_array()).ok_or_else(|| {
            Error::BadRequest("ChatGPT Web requires messages array".into())
        })?;

        let (has_tools, requested_tools, effective_messages) =
            prepare_tool_messages(&req.body, messages);

        let mut turn_body = req.body.clone();
        if let Some(obj) = turn_body.as_object_mut() {
            obj.insert("messages".to_string(), Json::Array(effective_messages));
        }

        let requested_tool_names: Vec<String> = requested_tools
            .as_ref()
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|t| t.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let turn = run_chat_gpt_web_http_turn(
            CHATGPT_WEB_URL.to_string(),
            access_token,
            cookie,
            user_agent,
            &turn_body,
            has_tools,
            requested_tool_names,
            None,
        )
        .await
        .map_err(|e| Error::other(format!("ChatGPT HTTP turn failed: {}", e)))?;

        let cid = format!("chatcmpl-{}", Uuid::new_v4());
        let created = Utc::now().timestamp();

        let response = if !turn.has_tools {
            build_openai_response(req.model, &turn.text, req.stream, &cid, created)
        } else {
            let tools_slice = requested_tools.as_deref().unwrap_or(&[]);
            let (content, tool_calls, finish_reason) =
                build_tool_aware_result(&turn.text, tools_slice, "call");
            build_tool_mode_response(
                req.model,
                &content,
                tool_calls,
                finish_reason,
                req.stream,
                &cid,
                created,
            )
        };

        Ok(ExecResult {
            response,
            url: CHATGPT_WEB_URL.to_string(),
            headers: HeaderList::new(),
            transformed_body: req.body,
            response_format: None,
        })
    }
}

/// Extract Cookie header string from credentials (apiKey, storageState, or raw cookie).
fn extract_cookie(creds: &Credentials) -> Result<String> {
    let raw = creds
        .psd_str("storageState")
        .or_else(|| creds.api_key())
        .or_else(|| creds.str("cookie"))
        .ok_or_else(|| Error::BadRequest("Missing ChatGPT Web credentials or session cookie".into()))?;

    let trimmed = raw.trim();
    if trimmed.starts_with('{') {
        if let Ok(parsed) = serde_json::from_str::<Json>(trimmed) {
            if let Some(cookies) = parsed.get("cookies").and_then(|c| c.as_array()) {
                let parts: Vec<String> = cookies
                    .iter()
                    .filter_map(|c| {
                        let name = c.get("name")?.as_str()?;
                        let value = c.get("value")?.as_str()?;
                        Some(format!("{}={}", name, value))
                    })
                    .collect();
                if !parts.is_empty() {
                    return Ok(parts.join("; "));
                }
            }
        }
    }

    if !trimmed.contains('=') {
        return Ok(format!("__Secure-next-auth.session-token={}", trimmed));
    }

    Ok(trimmed.to_string())
}

/// Fetch access token from `/api/auth/session` using the Cookie header.
async fn fetch_access_token(cookie: &str, user_agent: &str) -> Result<String> {
    let client = reqwest::Client::new();
    let res = client
        .get(format!("{}/api/auth/session", CHATGPT_WEB_URL))
        .header("User-Agent", user_agent)
        .header("Accept", "application/json")
        .header("Cookie", cookie)
        .send()
        .await
        .map_err(|e| Error::Network(format!("Failed to connect to ChatGPT auth session: {}", e)))?;

    if !res.status().is_success() {
        return Err(Error::other(format!(
            "ChatGPT session unavailable (HTTP {})",
            res.status()
        )));
    }

    let json_val = res
        .json::<Json>()
        .await
        .map_err(|e| Error::other(format!("Invalid auth session response: {}", e)))?;

    let token = json_val
        .get("accessToken")
        .and_then(|t| t.as_str())
        .ok_or_else(|| Error::other("ChatGPT session is not signed in"))?;

    Ok(token.to_string())
}

fn build_openai_response(
    model: &str,
    text: &str,
    stream: bool,
    cid: &str,
    created: i64,
) -> UpstreamResponse {
    if !stream {
        let body = json!({
            "id": cid,
            "object": "chat.completion",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": text },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 }
        });
        return UpstreamResponse::json(200, &body);
    }

    let mut output = String::new();
    let c1 = json!({
        "id": cid,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{ "index": 0, "delta": { "role": "assistant" }, "finish_reason": Json::Null }]
    });
    output.push_str(&sse_chunk(&c1));

    let c2 = json!({
        "id": cid,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{ "index": 0, "delta": { "content": text }, "finish_reason": Json::Null }]
    });
    output.push_str(&sse_chunk(&c2));

    let c3 = json!({
        "id": cid,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }]
    });
    output.push_str(&sse_chunk(&c3));
    output.push_str("data: [DONE]\n\n");

    let stream = Box::pin(futures::stream::once(async move {
        Ok::<Bytes, std::io::Error>(Bytes::from(output))
    }));
    UpstreamResponse::sse(stream)
}

fn build_tool_mode_response(
    model: &str,
    content: &str,
    tool_calls: Option<Vec<ToolCall>>,
    finish_reason: &str,
    stream: bool,
    cid: &str,
    created: i64,
) -> UpstreamResponse {
    let has_calls = tool_calls.as_ref().map(|c| !c.is_empty()).unwrap_or(false);

    if !stream {
        let mut msg = json!({ "role": "assistant" });
        if has_calls {
            let tc_json: Vec<Json> = tool_calls
                .unwrap()
                .into_iter()
                .map(|tc| {
                    json!({
                        "id": tc.id,
                        "type": "function",
                        "function": {
                            "name": tc.function.name,
                            "arguments": tc.function.arguments
                        }
                    })
                })
                .collect();
            msg["tool_calls"] = Json::Array(tc_json);
            msg["content"] = if content.is_empty() { Json::Null } else { Json::String(content.to_string()) };
        } else {
            msg["content"] = Json::String(content.to_string());
        }

        let body = json!({
            "id": cid,
            "object": "chat.completion",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "message": msg,
                "finish_reason": finish_reason
            }],
            "usage": { "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 }
        });
        return UpstreamResponse::json(200, &body);
    }

    let mut output = String::new();
    let c1 = json!({
        "id": cid,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{ "index": 0, "delta": { "role": "assistant" }, "finish_reason": Json::Null }]
    });
    output.push_str(&sse_chunk(&c1));

    if !content.is_empty() {
        let c_text = json!({
            "id": cid,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{ "index": 0, "delta": { "content": content }, "finish_reason": Json::Null }]
        });
        output.push_str(&sse_chunk(&c_text));
    }

    if let Some(calls) = tool_calls {
        for (idx, tc) in calls.into_iter().enumerate() {
            let c_tc = json!({
                "id": cid,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": {
                        "tool_calls": [{
                            "index": idx,
                            "id": tc.id,
                            "type": "function",
                            "function": {
                                "name": tc.function.name,
                                "arguments": tc.function.arguments
                            }
                        }]
                    },
                    "finish_reason": Json::Null
                }]
            });
            output.push_str(&sse_chunk(&c_tc));
        }
    }

    let c_fin = json!({
        "id": cid,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{ "index": 0, "delta": {}, "finish_reason": finish_reason }]
    });
    output.push_str(&sse_chunk(&c_fin));
    output.push_str("data: [DONE]\n\n");

    let stream = Box::pin(futures::stream::once(async move {
        Ok::<Bytes, std::io::Error>(Bytes::from(output))
    }));
    UpstreamResponse::sse(stream)
}

/// Create the executor instance
pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(ChatGptWebExecutor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_name() {
        let exec = ChatGptWebExecutor;
        assert_eq!(exec.provider(), "chatgpt-web");
    }

    #[test]
    fn test_extract_cookie_raw_session_token() {
        let mut creds = Credentials::new();
        creds.set("apiKey", "raw-token-12345");
        let cookie = extract_cookie(&creds).unwrap();
        assert_eq!(cookie, "__Secure-next-auth.session-token=raw-token-12345");
    }

    #[test]
    fn test_extract_cookie_full_header() {
        let mut creds = Credentials::new();
        creds.set("apiKey", "__Secure-next-auth.session-token=tok; cf_clearance=cf1");
        let cookie = extract_cookie(&creds).unwrap();
        assert_eq!(cookie, "__Secure-next-auth.session-token=tok; cf_clearance=cf1");
    }

    #[test]
    fn test_extract_cookie_storage_state_json() {
        let mut creds = Credentials::new();
        creds.set(
            "apiKey",
            r#"{"cookies": [{"name": "__Secure-next-auth.session-token", "value": "my-token"}]}"#,
        );
        let cookie = extract_cookie(&creds).unwrap();
        assert_eq!(cookie, "__Secure-next-auth.session-token=my-token");
    }
}
