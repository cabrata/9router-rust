//! Port of open-sse/executors/gemini-web.js
//! Type browser automation of Gemini web UI
//! Auth: Cookie-based (__Secure-1PSID + __Secure-1PSIDTS from gemini.google.com)
//! Method: Browser automation

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;

use nr_core::http::{HeaderList, UpstreamResponse};
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

/// Composer selector: the readiness check and the input target.
pub const COMPOSER_SELECTOR: &str = ".ql-editor, [contenteditable='true']";
/// Assistant bubbles. `model-response` is the current contract; `message-content`
/// is the older wrapper Google still renders on some accounts.
const ASSISTANT_SELECTOR: &str = "model-response, message-content";

const POLL_INTERVAL_MS: u64 = 400;
// Unchanged text for this many polls (with the thinking overlay gone) means the
// turn finished.
const STABLE_POLLS_DONE: u32 = 5;
// Safety valve: finish even if the overlay never clears (UI change).
const STABLE_POLLS_FORCE: u32 = 20;
const FIRST_TOKEN_TIMEOUT_MS: u64 = 30_000;

const READ_REPLY_STATE: &str = r#"(() => {
  const msgs = [...document.querySelectorAll("model-response, message-content")];
  const last = msgs.length ? (msgs[msgs.length - 1].innerText || '').trim() : '';
  return {
    count: msgs.length,
    lastText: last,
    generating: !!document.querySelector('[data-test-id="thinking-overlay-content"], button[aria-label*="Stop" i]'),
  };
})()"#;

/// What the assistant bubbles currently show.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReplyState {
    pub count: usize,
    pub last_text: String,
    /// The "thinking"/stop button is visible, so the turn is still streaming.
    pub generating: bool,
}

/// `readReplyState(page)` — a navigating page reads as "no change yet".
pub async fn read_reply_state(page: &Arc<dyn crate::cdp::Page>) -> ReplyState {
    let Ok(value) = page.evaluate(READ_REPLY_STATE, None).await else {
        return ReplyState::default();
    };
    ReplyState {
        count: value.get("count").and_then(Json::as_u64).unwrap_or(0) as usize,
        last_text: value
            .get("lastText")
            .and_then(Json::as_str)
            .unwrap_or("")
            .trim()
            .to_string(),
        generating: value.get("generating").and_then(Json::as_bool).unwrap_or(false),
    }
}

/// `waitForGeminiReply` — poll the assistant bubbles for the reply that follows
/// the just-sent message and return its full text.
///
/// Interim bubble texts ("Searching the web" …) are replaced wholesale by the
/// real answer while the thinking overlay is up, so `on_delta` is only called
/// once the overlay is gone: a client must never receive an interim status as
/// content.
pub async fn poll_reply<F: FnMut(&str)>(
    page: &Arc<dyn crate::cdp::Page>,
    base: &ReplyState,
    timeout_ms: u64,
    mut on_delta: F,
) -> Result<String, String> {
    let started = tokio::time::Instant::now();
    let mut emitted = String::new();
    let mut last: Option<String> = None;
    let mut stable = 0u32;
    let mut got_first = false;

    loop {
        let elapsed = started.elapsed().as_millis() as u64;
        if elapsed > timeout_ms {
            return Err("Gemini reply timed out".into());
        }
        if !got_first && elapsed > FIRST_TOKEN_TIMEOUT_MS {
            return Err("no first token from Gemini".into());
        }
        tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;

        let state = read_reply_state(page).await;
        let text = state.last_text.clone();
        let is_new = state.count > base.count || (!text.is_empty() && text != base.last_text);
        if !is_new || text.is_empty() {
            continue;
        }

        got_first = true;
        if last.as_deref() != Some(text.as_str()) {
            last = Some(text.clone());
            stable = 0;
        } else {
            stable += 1;
        }

        if !state.generating && text != emitted {
            let delta = match text.strip_prefix(emitted.as_str()) {
                Some(rest) => rest.to_string(),
                // A rewrite (not an append) replaces the visible text.
                None => text.clone(),
            };
            if !delta.is_empty() {
                emitted = text.clone();
                on_delta(&delta);
            }
        }

        if stable >= STABLE_POLLS_FORCE {
            return Ok(if emitted.is_empty() { text } else { emitted });
        }
        if !state.generating && stable >= STABLE_POLLS_DONE && text == emitted {
            return Ok(emitted);
        }
    }
}

/// SSE headers with proxy buffering disabled (the JS `SSE_HEADERS_NO_BUFFER`).
fn sse_headers() -> http::HeaderMap {
    let mut headers = http::HeaderMap::new();
    for (k, v) in nr_common::sse_constants::SSE_HEADERS_NO_BUFFER {
        if let (Ok(name), Ok(value)) = (
            http::header::HeaderName::try_from(*k),
            http::header::HeaderValue::try_from(*v),
        ) {
            headers.insert(name, value);
        }
    }
    headers
}

/// One OpenAI SSE chunk carrying a content delta (or the terminal frame).
pub fn format_stream_chunk(content: &str, model: &str) -> String {
    json!({
        "id": "chatcmpl-gwe",
        "object": "chat.completion.chunk",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
        "choices": [{ "index": 0, "delta": { "content": content }, "finish_reason": Json::Null }]
    })
    .to_string()
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
            // The Google session cookies go on the context, not the page, so
            // every navigation (including the login redirect) carries them.
            cookie_string: Some(cookie.clone()),
            cookie_domain: Some(".google.com".to_string()),
            ..Default::default()
        };

        match crate::services::acquire_browser_context(&key, context_opts).await {
            Ok(context) => {
                match context.new_page().await {
                    Ok(page) => {
                        // Open the app, wait for the composer, send, then poll
                        // the assistant bubbles for the reply.
                        let goto_opts = crate::cdp::GotoOptions {
                            wait_until: "domcontentloaded".to_string(),
                            timeout_ms: 20_000,
                        };
                        if let Err(e) = page.goto(GEMINI_URL, goto_opts).await {
                            return Ok(ExecResult {
                                response: UpstreamResponse::json(
                                    502,
                                    &json!({ "error": { "message": format!("gemini-web navigation failed: {e}"), "type": "browser_error" } }),
                                ),
                                url: GEMINI_URL.to_string(),
                                headers: HeaderList::new(),
                                transformed_body: req.body,
                                response_format: None,
                            });
                        }

                        // The composer selector is the readiness check; a
                        // bounced session lands on the marketing splash instead.
                        if let Err(e) = page.wait_for_selector(COMPOSER_SELECTOR, 10_000).await {
                            return Ok(ExecResult {
                                response: UpstreamResponse::json(
                                    502,
                                    &json!({
                                        "error": {
                                            "message": format!(
                                                "Gemini did not open a chat session ({e}) — sign in to Google again via the dashboard, and make sure Gemini Apps Activity is enabled"
                                            ),
                                            "type": "browser_error",
                                        }
                                    }),
                                ),
                                url: GEMINI_URL.to_string(),
                                headers: HeaderList::new(),
                                transformed_body: req.body,
                                response_format: None,
                            });
                        }

                        let base = read_reply_state(&page).await;
                        if let Err(e) = page.click(COMPOSER_SELECTOR).await {
                            return Ok(ExecResult {
                                response: UpstreamResponse::json(
                                    502,
                                    &json!({ "error": { "message": format!("gemini-web composer click failed: {e}"), "type": "browser_error" } }),
                                ),
                                url: GEMINI_URL.to_string(),
                                headers: HeaderList::new(),
                                transformed_body: req.body,
                                response_format: None,
                            });
                        }
                        if let Err(e) = page.insert_text(&prompt).await {
                            return Ok(ExecResult {
                                response: UpstreamResponse::json(
                                    502,
                                    &json!({ "error": { "message": format!("gemini-web prompt entry failed: {e}"), "type": "browser_error" } }),
                                ),
                                url: GEMINI_URL.to_string(),
                                headers: HeaderList::new(),
                                transformed_body: req.body,
                                response_format: None,
                            });
                        }
                        if let Err(e) = page.press_key("Enter").await {
                            return Ok(ExecResult {
                                response: UpstreamResponse::json(
                                    502,
                                    &json!({ "error": { "message": format!("gemini-web send failed: {e}"), "type": "browser_error" } }),
                                ),
                                url: GEMINI_URL.to_string(),
                                headers: HeaderList::new(),
                                transformed_body: req.body,
                                response_format: None,
                            });
                        }

                        if req.stream {
                            // Move the page into the poll loop so deltas reach
                            // the client while the bubble is still growing.
                            let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
                            let model_owned = model_id.to_string();
                            tokio::spawn(async move {
                                let cid = format!("chatcmpl-gwe-{}", &uuid::Uuid::new_v4().to_string()[..12]);
                                let created = chrono::Utc::now().timestamp();
                                let mut emitted = String::new();
                                let mut failed = false;

                                let outcome = poll_reply(&page, &base, 60_000, |delta| {
                                    let chunk = format_stream_chunk(delta, &model_owned);
                                    let _ = tx.try_send(Ok(Bytes::from(format!("data: {chunk}\n\n"))));
                                })
                                .await;

                                match outcome {
                                    Ok(text) => emitted.push_str(&text),
                                    Err(e) => {
                                        failed = true;
                                        let err = json!({ "error": { "message": e } });
                                        let _ = tx.send(Ok(Bytes::from(format!("data: {err}\n\ndata: [DONE]\n\n")))).await;
                                    }
                                }

                                if !failed {
                                    let stop = json!({
                                        "id": cid,
                                        "object": "chat.completion.chunk",
                                        "created": created,
                                        "model": model_owned,
                                        "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }]
                                    });
                                    let _ = tx.send(Ok(Bytes::from(format!("data: {stop}\n\ndata: [DONE]\n\n")))).await;
                                }
                                let _ = emitted;
                                let _ = page.close().await;
                            });

                            return Ok(ExecResult {
                                response: UpstreamResponse::from_stream(
                                    200,
                                    sse_headers(),
                                    Box::pin(futures::stream::poll_fn({
                                        let mut rx = rx;
                                        move |cx| rx.poll_recv(cx)
                                    })),
                                ),
                                url: GEMINI_URL.to_string(),
                                headers: HeaderList::new(),
                                transformed_body: req.body,
                                response_format: None,
                            });
                        }

                        match poll_reply(&page, &base, 60_000, |_| {}).await {
                            Ok(text) => {
                                let completion = format_chat_completion(&text, model_id);
                                let _ = page.close().await;
                                return Ok(ExecResult {
                                    response: UpstreamResponse::json(200, &completion),
                                    url: GEMINI_URL.to_string(),
                                    headers: HeaderList::new(),
                                    transformed_body: req.body,
                                    response_format: None,
                                });
                            }
                            Err(e) => {
                                let _ = page.close().await;
                                return Ok(ExecResult {
                                    response: UpstreamResponse::json(
                                        502,
                                        &json!({ "error": { "message": e, "type": "browser_error" } }),
                                    ),
                                    url: GEMINI_URL.to_string(),
                                    headers: HeaderList::new(),
                                    transformed_body: req.body,
                                    response_format: None,
                                });
                            }
                        }
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
