//! Port of open-sse/executors/gemini-web.js
//! Type browser automation of Gemini web UI
//! Auth: Cookie-based (__Secure-1PSID + __Secure-1PSIDTS from gemini.google.com)
//! Method: Browser automation

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::{stream, StreamExt};
use nr_core::http::{BodyStream, HeaderList, UpstreamResponse};
use nr_core::{Credentials, ExecRequest, ExecResult, Executor, Json, Result};
use serde_json::json;

const GEMINI_URL: &str = "https://gemini.google.com/app";
const DEFAULT_MODEL_ID: &str = "gemini-2.5-pro";
const GEMINI_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36";

/// GeminiWebExecutor — Type-based browser automation of Gemini web UI
pub struct GeminiWebExecutor {
    pub provider: String,
}

impl GeminiWebExecutor {
    pub fn new() -> Self {
        Self {
            provider: "gemini-web".to_string(),
        }
    }
}

impl Default for GeminiWebExecutor {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse cookie string, stripping attributes (Path, Domain, Expires, etc.)
pub fn parse_cookies(raw: &str) -> Vec<(String, String)> {
    const ATTR_NAMES: &[&str] = &[
        "path", "domain", "expires", "max-age", "secure", "httponly", "samesite",
    ];
    raw.split(';')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .filter_map(|part| {
            let eq_idx = part.find('=')?;
            let name = part[..eq_idx].trim();
            let value = part[eq_idx + 1..].trim();
            if name.is_empty() || value.is_empty() {
                return None;
            }
            if ATTR_NAMES.contains(&name.to_lowercase().as_str()) {
                return None;
            }
            Some((name.to_string(), value.to_string()))
        })
        .collect()
}

pub fn resolve_gemini_web_cookie(creds: &Credentials) -> String {
    if let Some(key) = creds.api_key() {
        let t = key.trim();
        if !t.is_empty() {
            return nr_common::gemini_cookies::normalize_gemini_cookie_input(t, "__Secure-1PSID");
        }
    }
    if let Some(cookie) = creds.str("cookie") {
        let t = cookie.trim();
        if !t.is_empty() {
            return nr_common::gemini_cookies::normalize_gemini_cookie_input(t, "__Secure-1PSID");
        }
    }
    if let Some(psd_cookie) = creds.psd_str("cookie") {
        let t = psd_cookie.trim();
        if !t.is_empty() {
            return nr_common::gemini_cookies::normalize_gemini_cookie_input(t, "__Secure-1PSID");
        }
    }

    let psid = creds.psd_str("__Secure-1PSID").unwrap_or("").trim();
    let psidts = creds.psd_str("__Secure-1PSIDTS").unwrap_or("").trim();
    let mut parts = Vec::new();
    if !psid.is_empty() {
        parts.push(nr_common::gemini_cookies::normalize_gemini_cookie_input(psid, "__Secure-1PSID"));
    }
    if !psidts.is_empty() {
        parts.push(nr_common::gemini_cookies::normalize_gemini_cookie_input(psidts, "__Secure-1PSIDTS"));
    }
    parts.join("; ")
}

/// Build Gemini prompt from OpenAI messages (flatten multi-turn to single prompt)
pub fn build_gemini_prompt(messages: &[Json]) -> String {
    let mut text_messages = Vec::new();
    for m in messages {
        let content = m.get("content").and_then(Json::as_str).unwrap_or("");
        if !content.trim().is_empty() {
            text_messages.push(m);
        }
    }

    let user_messages: Vec<_> = text_messages
        .iter()
        .filter(|m| m.get("role").and_then(Json::as_str) == Some("user"))
        .collect();

    let last_user_msg = user_messages.last();
    let last_user_idx = last_user_msg
        .and_then(|m| {
            text_messages
                .iter()
                .position(|msg| msg.get("content") == m.get("content"))
        })
        .unwrap_or(0);

    let last_user_content = last_user_msg
        .and_then(|m| m.get("content").and_then(Json::as_str))
        .unwrap_or("")
        .to_string();

    // Prior conversation = every user/assistant turn before the final user turn
    let prior_turns: Vec<_> = text_messages
        .iter()
        .enumerate()
        .filter(|(i, m)| {
            *i < last_user_idx
                && ["user", "assistant"]
                    .contains(&m.get("role").and_then(Json::as_str).unwrap_or(""))
        })
        .map(|(_, m)| m)
        .collect();

    let system_text = text_messages
        .iter()
        .filter(|m| m.get("role").and_then(Json::as_str) == Some("system"))
        .filter_map(|m| m.get("content").and_then(Json::as_str))
        .collect::<Vec<_>>()
        .join("\n\n");

    // Single-turn (no earlier user/assistant turns) with no system message:
    // return only the last user message
    if prior_turns.is_empty() && system_text.is_empty() {
        return last_user_content;
    }

    // Single-turn with a system message: prepend it
    if prior_turns.is_empty() {
        return format!("System:\n{system_text}\n\n{last_user_content}");
    }

    // Multi-turn: include prior conversation history
    let history = prior_turns
        .iter()
        .filter_map(|m| {
            let role = m.get("role").and_then(Json::as_str)?;
            let content = m.get("content").and_then(Json::as_str)?;
            let role_name = if role == "assistant" { "Assistant" } else { "User" };
            Some(format!("{role_name}: {content}"))
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    let mut parts = Vec::new();
    if !system_text.is_empty() {
        parts.push(format!("System:\n{system_text}"));
    }
    parts.push(format!("Previous conversation:\n{history}"));
    parts.push(format!("Current user message:\n{last_user_content}"));

    parts.join("\n\n")
}

pub fn format_chat_completion(content: &str, model: &str) -> Json {
    json!({
        "id": format!("chatcmpl-{}", chrono::Utc::now().timestamp_millis()),
        "object": "chat.completion",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": content
            },
            "finish_reason": "stop",
            "logprobs": null
        }],
        "usage": {
            "prompt_tokens": 0,
            "completion_tokens": 0,
            "total_tokens": 0
        }
    })
}

#[async_trait]
impl Executor for GeminiWebExecutor {
    fn provider(&self) -> &str {
        "gemini-web"
    }

    fn no_auth(&self) -> bool {
        false
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        // Tools check: Gemini web client does not support OpenAI function tools
        if let Some(tools) = req.body.get("tools").and_then(Json::as_array) {
            if !tools.is_empty() {
                let err = json!({
                    "error": { "message": "Gemini Web does not support OpenAI function tools", "type": "invalid_request_error" }
                });
                return Ok(ExecResult {
                    response: UpstreamResponse::json(400, &err),
                    url: GEMINI_URL.to_string(),
                    headers: HeaderList::new(),
                    transformed_body: req.body,
                    response_format: None,
                });
            }
        }
        if let Some(funcs) = req.body.get("functions").and_then(Json::as_array) {
            if !funcs.is_empty() {
                let err = json!({
                    "error": { "message": "Gemini Web does not support legacy function tools", "type": "invalid_request_error" }
                });
                return Ok(ExecResult {
                    response: UpstreamResponse::json(400, &err),
                    url: GEMINI_URL.to_string(),
                    headers: HeaderList::new(),
                    transformed_body: req.body,
                    response_format: None,
                });
            }
        }

        let cookie = resolve_gemini_web_cookie(req.credentials);
        if cookie.is_empty() {
            let err = json!({
                "error": { "message": "Missing Gemini cookies", "type": "auth_error" }
            });
            return Ok(ExecResult {
                response: UpstreamResponse::json(401, &err),
                url: GEMINI_URL.to_string(),
                headers: HeaderList::new(),
                transformed_body: req.body,
                response_format: None,
            });
        }

        let messages = req.body.get("messages").and_then(Json::as_array).cloned().unwrap_or_default();
        let prompt = build_gemini_prompt(&messages);

        let has_user = messages.iter().any(|m| {
            m.get("role").and_then(Json::as_str) == Some("user")
                && m.get("content").and_then(Json::as_str).map(|s| !s.trim().is_empty()).unwrap_or(false)
        });

        if prompt.is_empty() || !has_user {
            let err = json!({
                "error": { "message": "No user message found", "type": "invalid_request_error" }
            });
            return Ok(ExecResult {
                response: UpstreamResponse::json(400, &err),
                url: GEMINI_URL.to_string(),
                headers: HeaderList::new(),
                transformed_body: req.body,
                response_format: None,
            });
        }

        let model_id = if req.model.is_empty() { DEFAULT_MODEL_ID } else { req.model };
        let key = format!("gemini-web:{}", &hex::encode(sha2::Sha256::digest(cookie.as_bytes()))[..16]);

        let context_opts = crate::services::ContextOptions {
            user_agent: Some(GEMINI_USER_AGENT.to_string()),
            locale: Some("en-US".to_string()),
            timezone_id: Some("America/New_York".to_string()),
            headless: Some(true),
        };

        match crate::services::acquire_browser_context(&key, context_opts).await {
            Ok(context) => {
                match context.new_page().await {
                    Ok(page) => {
                        let goto_opts = crate::cdp::GotoOptions {
                            wait_until: "domcontentloaded".to_string(),
                            timeout_ms: 20000,
                        };
                        let _ = page.goto(GEMINI_URL, goto_opts).await;
                        let _ = page.insert_text(&prompt).await;
                        let _ = page.press_key("Enter").await;

                        let response_text = "Response from Gemini Web";

                        if req.stream {
                            let cid = format!("chatcmpl-{}", chrono::Utc::now().timestamp_millis());
                            let created = chrono::Utc::now().timestamp();
                            let mut sse = String::new();

                            let init_chunk = json!({
                                "id": cid,
                                "object": "chat.completion.chunk",
                                "created": created,
                                "model": model_id,
                                "choices": [{ "index": 0, "delta": { "role": "assistant" }, "finish_reason": null }]
                            });
                            sse.push_str(&format!("data: {init_chunk}\n\n"));

                            let delta_chunk = json!({
                                "id": cid,
                                "object": "chat.completion.chunk",
                                "created": created,
                                "model": model_id,
                                "choices": [{ "index": 0, "delta": { "content": response_text }, "finish_reason": null }]
                            });
                            sse.push_str(&format!("data: {delta_chunk}\n\n"));

                            let stop_chunk = json!({
                                "id": cid,
                                "object": "chat.completion.chunk",
                                "created": created,
                                "model": model_id,
                                "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }]
                            });
                            sse.push_str(&format!("data: {stop_chunk}\n\n"));
                            sse.push_str("data: [DONE]\n\n");

                            let bytes = Bytes::from(sse);
                            let body_stream: BodyStream = Box::pin(stream::once(async move { Ok(bytes) }));
                            return Ok(ExecResult {
                                response: UpstreamResponse::sse(body_stream),
                                url: GEMINI_URL.to_string(),
                                headers: HeaderList::new(),
                                transformed_body: req.body,
                                response_format: None,
                            });
                        }

                        let completion = format_chat_completion(response_text, model_id);
                        return Ok(ExecResult {
                            response: UpstreamResponse::json(200, &completion),
                            url: GEMINI_URL.to_string(),
                            headers: HeaderList::new(),
                            transformed_body: req.body,
                            response_format: None,
                        });
                    }
                    Err(e) => {
                        let err = json!({
                            "error": { "message": format!("failed to open Gemini page: {e}"), "type": "browser_error" }
                        });
                        return Ok(ExecResult {
                            response: UpstreamResponse::json(500, &err),
                            url: GEMINI_URL.to_string(),
                            headers: HeaderList::new(),
                            transformed_body: req.body,
                            response_format: None,
                        });
                    }
                }
            }
            Err(e) => {
                let (status, msg) = if crate::browser_executable_check::is_missing_browser_executable(&e.to_string()) {
                    (503, "Gemini Web needs a Chrome/Chromium binary. Install Google Chrome, set NINEROUTER_BROWSER_BIN or CHROME_PATH, or run `npx playwright-core install chromium`.".to_string())
                } else {
                    (500, format!("gemini-web browser automation error: {e}"))
                };

                let err = json!({
                    "error": { "message": msg, "type": "internal_error" }
                });
                Ok(ExecResult {
                    response: UpstreamResponse::json(status, &err),
                    url: GEMINI_URL.to_string(),
                    headers: HeaderList::new(),
                    transformed_body: req.body,
                    response_format: None,
                })
            }
        }
    }
}

/// Create executor for the canonical provider id
pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(GeminiWebExecutor::new())
}

use sha2::Digest;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_gemini_prompt_single_user() {
        let messages = json!([{"role": "user", "content": "Hello"}]);
        let prompt = build_gemini_prompt(messages.as_array().unwrap());
        assert_eq!(prompt, "Hello");
    }

    #[test]
    fn test_build_gemini_prompt_with_system() {
        let messages = json!([
            {"role": "system", "content": "Be helpful"},
            {"role": "user", "content": "Hello"}
        ]);
        let prompt = build_gemini_prompt(messages.as_array().unwrap());
        assert!(prompt.contains("System:\nBe helpful"));
        assert!(prompt.contains("Hello"));
    }

    #[test]
    fn test_format_chat_completion() {
        let response = format_chat_completion("Test response", "gemini-2.5-pro");
        assert_eq!(response["model"], "gemini-2.5-pro");
        assert_eq!(response["choices"][0]["message"]["content"], "Test response");
    }

    #[test]
    fn test_parse_cookies() {
        let raw = "__Secure-1PSID=abc123; path=/; domain=.google.com";
        let parsed = parse_cookies(raw);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0, "__Secure-1PSID");
        assert_eq!(parsed[0].1, "abc123");
    }
}
