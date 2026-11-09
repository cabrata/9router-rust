//! Port of open-sse/executors/muse-web.js + companions
//! muse.ai Hatch agent via browser automation + /api/session renewal
//! Auth: Cookie-based session renewal via /api/session
//! Response: Poll agent bubbles, handle approval cards, extract media

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::{stream, StreamExt};
use nr_core::http::{BodyStream, HeaderList, UpstreamResponse};
use nr_core::{ExecRequest, ExecResult, Executor, Json, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const SITE: &str = "https://muse.ai";
pub const NEW_THREAD_URL: &str = "https://muse.ai/thread/new";
pub const SESSION_API: &str = "https://muse.ai/api/session";
pub const VM_WAKE_API: &str = "https://muse.ai/api/hatch/vm/wake";

const DEFAULT_MODEL_ID: &str = "muse-chat";
const MUSE_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";

const COOKIE_ATTR_NAMES: &[&str] = &[
    "path", "domain", "expires", "max-age", "secure", "httponly", "samesite",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_status: Option<String>,
    pub vm_id: Option<String>,
    pub vm_state: Option<String>,
    pub vm_wake_ok: Option<bool>,
    pub warning: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RenewedSession {
    pub cookie_header: String,
    pub jar: HashMap<String, String>,
    pub meta: SessionMeta,
}

/// Parse raw Cookie header into name=value pairs, skipping attributes
pub fn parse_cookie_pairs(raw: &str) -> Vec<(String, String)> {
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
            if COOKIE_ATTR_NAMES.contains(&name.to_lowercase().as_str()) {
                return None;
            }
            Some((name.to_string(), value.to_string()))
        })
        .collect()
}

pub fn cookie_header_of(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ")
}

pub fn resolve_muse_cookie(creds: &nr_core::Credentials) -> String {
    if let Some(key) = creds.api_key() {
        let t = key.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    if let Some(cookie) = creds.str("cookie") {
        let t = cookie.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    if let Some(psd_cookie) = creds.psd_str("cookie") {
        let t = psd_cookie.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    String::new()
}

/// Renew muse.ai session over HTTP, receiving rotating cookies via Set-Cookie
pub async fn renew_session(raw_cookie_header: &str, wake_vm: bool) -> nr_core::Result<RenewedSession> {
    let mut jar: HashMap<String, String> = HashMap::new();
    for (name, value) in parse_cookie_pairs(raw_cookie_header) {
        jar.insert(name, value);
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(15000))
        .build()
        .unwrap_or_default();

    let cookie_str = cookie_header_of(&jar.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>());

    let res = client
        .get(SESSION_API)
        .header("User-Agent", MUSE_USER_AGENT)
        .header("Origin", SITE)
        .header("Referer", NEW_THREAD_URL)
        .header("Accept", "application/json")
        .header("Cookie", &cookie_str)
        .send()
        .await
        .map_err(|e| nr_core::Error::other(format!("muse.ai session endpoint unreachable: {e}")))?;

    let status = res.status().as_u16();
    if status == 401 || status == 403 {
        return Err(nr_core::Error::other(format!(
            "muse.ai session rejected (HTTP {status}); cookies are no longer valid"
        )));
    }
    if !res.status().is_success() {
        return Err(nr_core::Error::other(format!(
            "muse.ai session endpoint returned HTTP {status}"
        )));
    }

    // Merge Set-Cookie headers
    for cookie_val in res.headers().get_all(reqwest::header::SET_COOKIE) {
        if let Ok(line) = cookie_val.to_str() {
            if let Some(pair) = line.split(';').next() {
                if let Some((k, v)) = pair.split_once('=') {
                    if !v.trim().is_empty() {
                        jar.insert(k.trim().to_string(), v.trim().to_string());
                    }
                }
            }
        }
    }

    let body_json: Json = res.json().await.unwrap_or(Json::Null);
    let mut meta = SessionMeta {
        session_status: body_json.get("status").and_then(Json::as_str).map(String::from),
        vm_id: body_json.get("vm_id").and_then(Json::as_str).map(String::from),
        vm_state: body_json.get("vm_state").and_then(Json::as_str).map(String::from),
        vm_wake_ok: None,
        warning: None,
    };

    if wake_vm && meta.vm_id.is_some() && meta.vm_state.as_deref() != Some("DISABLED") {
        let wake_cookie_str = cookie_header_of(&jar.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>());
        let wake_payload = json!({
            "vm_id": meta.vm_id,
            "retry_count": 0,
            "connect_attempt_id": uuid::Uuid::new_v4().to_string(),
        });

        let wake_res = client
            .post(VM_WAKE_API)
            .header("User-Agent", MUSE_USER_AGENT)
            .header("Origin", SITE)
            .header("Referer", NEW_THREAD_URL)
            .header("Content-Type", "application/json")
            .header("Cookie", &wake_cookie_str)
            .json(&wake_payload)
            .send()
            .await;

        meta.vm_wake_ok = Some(wake_res.map(|r| r.status().is_success()).unwrap_or(false));
    }

    if let Some(ref st) = meta.session_status {
        if st != "assigned" {
            meta.warning = Some(format!("session status \"{st}\""));
        }
    }

    let renewed_header = cookie_header_of(&jar.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>());
    Ok(RenewedSession {
        cookie_header: renewed_header,
        jar,
        meta,
    })
}

/// Flatten OpenAI messages into prompt text and image references
pub fn flatten_messages(messages: &[Json]) -> (String, Vec<String>) {
    let mut images = Vec::new();
    let mut turns = Vec::new();

    for msg in messages {
        let role = msg.get("role").and_then(Json::as_str).unwrap_or("user");
        let mut text_parts = Vec::new();

        if let Some(s) = msg.get("content").and_then(Json::as_str) {
            text_parts.push(s.to_string());
        } else if let Some(arr) = msg.get("content").and_then(Json::as_array) {
            for part in arr {
                if let Some(t) = part.get("text").and_then(Json::as_str) {
                    text_parts.push(t.to_string());
                } else if part.get("type").and_then(Json::as_str) == Some("image_url") {
                    if let Some(url) = part.get("image_url").and_then(|u| u.get("url")).and_then(Json::as_str) {
                        images.push(url.to_string());
                    }
                }
            }
        }

        let combined = text_parts.join("\n").trim().to_string();
        if !combined.is_empty() {
            turns.push((role.to_string(), combined));
        }
    }

    if turns.is_empty() {
        return (String::new(), images);
    }

    if turns.len() == 1 && turns[0].0 == "user" {
        return (turns[0].1.clone(), images);
    }

    let mut blocks: Vec<String> = turns
        .iter()
        .map(|(role, text)| {
            let label = match role.as_str() {
                "system" | "developer" => "System",
                "assistant" => "Assistant",
                "tool" => "Tool",
                _ => "User",
            };
            format!("[{label}]\n{text}")
        })
        .collect();

    blocks.push(
        "[Instruction]\nContinue the conversation above and reply as the Assistant to the last User message. Output only the reply.".to_string()
    );

    (blocks.join("\n\n"), images)
}

// ── muse.ai DOM contract (port of executers/muse-web/dom.js) ────────────────

const MUSE_INPUT: &str = "textarea";
const MUSE_AGENT_BUBBLE: &str = "div[class*=\"hatch-agent-bubble-bg\"]";
const MUSE_STOP_BUTTON: &str = "button[aria-label*=\"Stop\" i]";
const MUSE_APPROVAL_CARD: &str = "[data-testid=\"hatch-inline-approval-card\"]";
const MUSE_LOGIN_HINTS: [&str; 4] = ["log in", "sign in", "create an account", "use another account"];
const MUSE_STALL_HINTS: [&str; 2] = ["Still sending", "Connecting..."];

const MUSE_POLL_INTERVAL_MS: u64 = 400;
const MUSE_STABLE_POLLS_DONE: u32 = 5;
const MUSE_STABLE_POLLS_FORCE: u32 = 20;
const MUSE_FIRST_TOKEN_TIMEOUT_MS: u64 = 30000;
const MUSE_CHAT_TIMEOUT_MS: u64 = 120000;
const MUSE_STALL_AFTER_MS: u64 = 6000;

const MUSE_PAGE_STATE: &str = r#"(() => {
  const body = document.body ? document.body.innerText : '';
  const hydration = document.querySelector('[data-hatch-shell-hydration-state]');
  return {
    ready: document.readyState === 'complete' && !!document.querySelector('textarea')
           && (!hydration || hydration.getAttribute('data-hatch-shell-hydration-state') === 'hydrated')
           && !body.includes('Connecting...'),
    hasInput: !!document.querySelector('textarea'),
    path: location.pathname,
    head: body.slice(0, 800),
  };
})()"#;

const MUSE_CHAT_STATE: &str = r#"(() => {
  const scrollers = [...document.querySelectorAll('*')].filter(e => {
    const s = getComputedStyle(e);
    return (s.overflowY === 'auto' || s.overflowY === 'scroll') && e.scrollHeight > e.clientHeight + 50;
  });
  scrollers.forEach(e => { e.scrollTop = e.scrollHeight; });
  const bubbles = [...document.querySelectorAll('div[class*="hatch-agent-bubble-bg"]')];
  const last = bubbles.length ? (bubbles[bubbles.length - 1].innerText || '').trim() : '';
  const tail = document.body ? document.body.innerText.slice(-600) : '';
  return { agentCount: bubbles.length, lastText: last,
           generating: !!document.querySelector('button[aria-label*="Stop" i]'),
           approval: !!document.querySelector('[data-testid="hatch-inline-approval-card"]'),
           tail };
})()"#;

/// Click "Deny" on every pending approval card; the agent then carries on
/// without the action (and the result stays private instead of becoming a
/// public link).
const MUSE_DENY_APPROVALS: &str = r#"(() => {
  const denied = [];
  for (const card of document.querySelectorAll('[data-testid="hatch-inline-approval-card"]')) {
    const btn = [...card.querySelectorAll('button,[role=button]')]
      .find(b => /^(deny|decline|reject|don.t allow)$/i.test((b.innerText || '').trim()));
    if (btn && !btn.disabled) { btn.click(); denied.push(true); }
  }
  return denied;
})()"#;

const MUSE_CLICK_SEND: &str = r#"(() => {
  const btn = [...document.querySelectorAll('button,[role=button]')]
    .filter(b => b.offsetParent !== null)
    .find(b => /send|发送/i.test((b.getAttribute('aria-label') || '') + ' ' + (b.getAttribute('data-testid') || '')));
  if (!btn) return 'missing';
  if (btn.disabled) return 'disabled';
  btn.click();
  return 'clicked';
})()"#;

/// Snapshot of the conversation used to detect new output after sending.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MuseChatState {
    pub agent_count: usize,
    pub last_text: String,
    pub generating: bool,
    pub approval: bool,
    pub tail: String,
}

async fn read_muse_chat_state(page: &Arc<dyn crate::cdp::Page>) -> MuseChatState {
    let Ok(value) = page.evaluate(MUSE_CHAT_STATE, None).await else {
        return MuseChatState::default();
    };
    MuseChatState {
        agent_count: value.get("agentCount").and_then(Json::as_u64).unwrap_or(0) as usize,
        last_text: value.get("lastText").and_then(Json::as_str).unwrap_or("").trim().to_string(),
        generating: value.get("generating").and_then(Json::as_bool).unwrap_or(false),
        approval: value.get("approval").and_then(Json::as_bool).unwrap_or(false),
        tail: value.get("tail").and_then(Json::as_str).unwrap_or("").to_string(),
    }
}

/// `waitForThreadReady` — poll until the Hatch shell hydrated and the composer
/// exists, naming a login redirect instead of timing out opaquely.
pub async fn wait_for_thread_ready(page: &Arc<dyn crate::cdp::Page>, timeout_ms: u64) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let mut last_head = String::new();
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        if let Ok(state) = page.evaluate(MUSE_PAGE_STATE, None).await {
            if state.get("ready").and_then(Json::as_bool).unwrap_or(false) {
                return Ok(());
            }
            last_head = state.get("head").and_then(Json::as_str).unwrap_or("").to_lowercase();
        }
    }
    if MUSE_LOGIN_HINTS.iter().any(|h| last_head.contains(h)) {
        return Err("muse.ai redirected to login; cookies are no longer valid".into());
    }
    Err("muse.ai page did not become ready".into())
}

/// `sendPrompt` — set the textarea through the native setter so React sees it,
/// then click Send (falling back to Enter).
pub async fn send_muse_prompt(page: &Arc<dyn crate::cdp::Page>, prompt: &str) -> Result<(), String> {
    let script = format!(
        r#"(() => {{
  const ta = document.querySelector('{MUSE_INPUT}');
  if (!ta) return {{ ok: false, err: 'no-input' }};
  ta.focus();
  const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value').set;
  setter.call(ta, {prompt});
  ta.dispatchEvent(new Event('input', {{ bubbles: true }}));
  return {{ ok: true, len: ta.value.length }};
}})()"#,
        prompt = serde_json::to_string(prompt).unwrap_or_else(|_| "\"\"".into())
    );
    let result = page.evaluate(&script, None).await?;
    if result.get("ok").and_then(Json::as_bool) != Some(true) {
        return Err(format!(
            "cannot fill muse.ai chat input: {}",
            result.get("err").and_then(Json::as_str).unwrap_or("unknown")
        ));
    }

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if page.evaluate(MUSE_CLICK_SEND, None).await.ok().and_then(|v| v.as_str().map(String::from)).as_deref()
            == Some("clicked")
        {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    // Fall back to Enter, then verify the composer actually cleared.
    page.press_key("Enter").await?;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let empty = page
        .evaluate("!(document.querySelector('textarea') || { value: '' }).value", None)
        .await
        .map(|v| crate::cdp::js_truthy(&v))
        .unwrap_or(false);
    if !empty {
        return Err("send button not found and Enter did not submit".into());
    }
    Ok(())
}

/// `pollChat` — wait for the assistant bubble that follows the sent prompt.
pub async fn poll_muse_chat<F: FnMut(&str)>(
    page: &Arc<dyn crate::cdp::Page>,
    base: &MuseChatState,
    mut on_delta: F,
) -> Result<String, String> {
    let started = tokio::time::Instant::now();
    let mut emitted = String::new();
    let mut last: Option<String> = None;
    let mut stable = 0u32;
    let mut got_first = false;

    loop {
        let elapsed = started.elapsed().as_millis() as u64;
        if elapsed > MUSE_CHAT_TIMEOUT_MS {
            return Err("assistant reply timed out".into());
        }
        if !got_first && elapsed > MUSE_FIRST_TOKEN_TIMEOUT_MS {
            return Err("no first token from assistant".into());
        }
        tokio::time::sleep(std::time::Duration::from_millis(MUSE_POLL_INTERVAL_MS)).await;

        let state = read_muse_chat_state(page).await;
        if state.approval {
            // Nobody answers the card here, so deny it and let the agent carry on.
            let _ = page.evaluate(MUSE_DENY_APPROVALS, None).await;
            continue;
        }

        let text = state.last_text.clone();
        let is_new = state.agent_count > base.agent_count
            || (!text.is_empty() && text != base.last_text);
        if !is_new || text.is_empty() {
            if elapsed > MUSE_STALL_AFTER_MS && MUSE_STALL_HINTS.iter().any(|h| state.tail.contains(h)) {
                return Err("upstream workspace is stuck connecting".into());
            }
            continue;
        }

        got_first = true;
        if last.as_deref() != Some(text.as_str()) {
            let delta = match text.strip_prefix(emitted.as_str()) {
                Some(rest) => rest.to_string(),
                None => text.clone(),
            };
            if !delta.is_empty() {
                emitted = text.clone();
                on_delta(&delta);
            }
            last = Some(text);
            stable = 0;
        } else {
            stable += 1;
            if (!state.generating && stable >= MUSE_STABLE_POLLS_DONE) || stable >= MUSE_STABLE_POLLS_FORCE {
                return Ok(emitted);
            }
        }
    }
}

/// MuseWebExecutor — muse.ai Hatch agent executor
pub struct MuseWebExecutor {
    pub provider: String,
}

impl MuseWebExecutor {
    pub fn new() -> Self {
        Self {
            provider: "muse-web".to_string(),
        }
    }
}

impl Default for MuseWebExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Executor for MuseWebExecutor {
    fn provider(&self) -> &str {
        "muse-web"
    }

    fn no_auth(&self) -> bool {
        false
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        // Fast-fail if tools are requested
        if let Some(tools) = req.body.get("tools").and_then(Json::as_array) {
            if !tools.is_empty() {
                let err = json!({
                    "error": { "message": "Muse Web does not support OpenAI function tools", "type": "invalid_request_error" }
                });
                return Ok(ExecResult {
                    response: UpstreamResponse::json(400, &err),
                    url: SITE.to_string(),
                    headers: HeaderList::new(),
                    transformed_body: req.body,
                    response_format: None,
                });
            }
        }
        if let Some(funcs) = req.body.get("functions").and_then(Json::as_array) {
            if !funcs.is_empty() {
                let err = json!({
                    "error": { "message": "Muse Web does not support legacy function tools", "type": "invalid_request_error" }
                });
                return Ok(ExecResult {
                    response: UpstreamResponse::json(400, &err),
                    url: SITE.to_string(),
                    headers: HeaderList::new(),
                    transformed_body: req.body,
                    response_format: None,
                });
            }
        }

        let cookie = resolve_muse_cookie(req.credentials);
        if cookie.is_empty() {
            let err = json!({
                "error": { "message": "Missing muse.ai cookies (hatch_sess required)", "type": "auth_error" }
            });
            return Ok(ExecResult {
                response: UpstreamResponse::json(401, &err),
                url: SITE.to_string(),
                headers: HeaderList::new(),
                transformed_body: req.body,
                response_format: None,
            });
        }

        let messages = req.body.get("messages").and_then(Json::as_array).cloned().unwrap_or_default();
        let (prompt, _images) = flatten_messages(&messages);
        if prompt.is_empty() {
            let err = json!({
                "error": { "message": "No user message found", "type": "invalid_request_error" }
            });
            return Ok(ExecResult {
                response: UpstreamResponse::json(400, &err),
                url: SITE.to_string(),
                headers: HeaderList::new(),
                transformed_body: req.body,
                response_format: None,
            });
        }

        // Renew session over HTTP
        let session = match renew_session(&cookie, true).await {
            Ok(s) => s,
            Err(e) => {
                let status = if e.to_string().contains("rejected") || e.to_string().contains("401") {
                    401
                } else {
                    502
                };
                let err = json!({
                    "error": { "message": e.to_string(), "type": "upstream_error" }
                });
                return Ok(ExecResult {
                    response: UpstreamResponse::json(status, &err),
                    url: SESSION_API.to_string(),
                    headers: HeaderList::new(),
                    transformed_body: req.body,
                    response_format: None,
                });
            }
        };

        let model = if req.model.is_empty() { DEFAULT_MODEL_ID } else { req.model };
        let cid = format!("chatcmpl-muw-{}", &uuid::Uuid::new_v4().to_string()[..12]);
        let created = chrono::Utc::now().timestamp();

        // Check if browser pool is available and launch browser context
        let key = format!("muse-web:{}", &hex::encode(sha2::Sha256::digest(cookie.as_bytes()))[..16]);
        let context_opts = crate::services::ContextOptions {
            user_agent: Some(MUSE_USER_AGENT.to_string()),
            locale: Some("en-US".to_string()),
            timezone_id: Some("America/New_York".to_string()),
            headless: Some(true),
            // The muse.ai session cookie authenticates the whole context.
            cookie_string: Some(cookie.clone()),
            cookie_domain: Some(".muse.ai".to_string()),
            ..Default::default()
        };

        // Try to acquire pooled browser context
        match crate::services::acquire_browser_context(&key, context_opts).await {
            Ok(context) => {
                // Navigate to thread new and drive browser
                match context.new_page().await {
                    Ok(page) => {
                        let goto_opts = crate::cdp::GotoOptions {
                            wait_until: "domcontentloaded".to_string(),
                            timeout_ms: 45000,
                        };
                        if let Err(e) = page.goto(NEW_THREAD_URL, goto_opts).await {
                            return Ok(ExecResult {
                                response: UpstreamResponse::json(
                                    502,
                                    &json!({ "error": { "message": format!("muse.ai navigation failed: {e}"), "type": "browser_error" } }),
                                ),
                                url: NEW_THREAD_URL.to_string(),
                                headers: HeaderList::new(),
                                transformed_body: req.body,
                                response_format: None,
                            });
                        }
                        if let Err(e) = wait_for_thread_ready(&page, 30_000).await {
                            return Ok(ExecResult {
                                response: UpstreamResponse::json(
                                    502,
                                    &json!({ "error": { "message": e, "type": "browser_error" } }),
                                ),
                                url: NEW_THREAD_URL.to_string(),
                                headers: HeaderList::new(),
                                transformed_body: req.body,
                                response_format: None,
                            });
                        }

                        let base = read_muse_chat_state(&page).await;
                        if let Err(e) = send_muse_prompt(&page, &prompt).await {
                            return Ok(ExecResult {
                                response: UpstreamResponse::json(
                                    502,
                                    &json!({ "error": { "message": e, "type": "browser_error" } }),
                                ),
                                url: NEW_THREAD_URL.to_string(),
                                headers: HeaderList::new(),
                                transformed_body: req.body,
                                response_format: None,
                            });
                        }

                        if req.stream {
                            let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
                            let cid_owned = cid.clone();
                            let model_owned = model.to_string();
                            tokio::spawn(async move {
                                let init = json!({
                                    "id": cid_owned,
                                    "object": "chat.completion.chunk",
                                    "created": created,
                                    "model": model_owned,
                                    "choices": [{ "index": 0, "delta": { "role": "assistant" }, "finish_reason": Json::Null }]
                                });
                                let _ = tx.send(Ok(Bytes::from(format!("data: {init}\n\n")))).await;

                                // Deltas must reach the client while the bubble is
                                // still growing, so the sink is a non-blocking send.
                                let sink = tx.clone();
                                let model_for_delta = model_owned.clone();
                                let cid_for_delta = cid_owned.clone();
                                let outcome = poll_muse_chat(&page, &base, |delta| {
                                    let chunk = json!({
                                        "id": cid_for_delta,
                                        "object": "chat.completion.chunk",
                                        "created": created,
                                        "model": model_for_delta,
                                        "choices": [{ "index": 0, "delta": { "content": delta }, "finish_reason": Json::Null }]
                                    });
                                    let _ = sink.try_send(Ok(Bytes::from(format!("data: {chunk}\n\n"))));
                                })
                                .await;

                                match outcome {
                                    Ok(_) => {
                                        let stop = json!({
                                            "id": cid_owned,
                                            "object": "chat.completion.chunk",
                                            "created": created,
                                            "model": model_owned,
                                            "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }]
                                        });
                                        let _ = tx.send(Ok(Bytes::from(format!("data: {stop}\n\ndata: [DONE]\n\n")))).await;
                                    }
                                    Err(e) => {
                                        let err = json!({ "error": { "message": e } });
                                        let _ = tx.send(Ok(Bytes::from(format!("data: {err}\n\ndata: [DONE]\n\n")))).await;
                                    }
                                }
                                let _ = page.close().await;
                            });

                            let headers = {
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
                            };
                            return Ok(ExecResult {
                                response: UpstreamResponse::from_stream(200, headers, Box::pin(futures::stream::poll_fn({
                                    let mut rx = rx;
                                    move |cx| rx.poll_recv(cx)
                                }))),
                                url: NEW_THREAD_URL.to_string(),
                                headers: HeaderList::new(),
                                transformed_body: req.body,
                                response_format: None,
                            });
                        }

                        match poll_muse_chat(&page, &base, |_| {}).await {
                            Ok(content) => {
                                let _ = page.close().await;
                                let completion = json!({
                                    "id": cid,
                                    "object": "chat.completion",
                                    "created": created,
                                    "model": model,
                                    "choices": [{
                                        "index": 0,
                                        "message": { "role": "assistant", "content": content },
                                        "finish_reason": "stop",
                                        "logprobs": Json::Null
                                    }],
                                    "usage": { "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 }
                                });
                                return Ok(ExecResult {
                                    response: UpstreamResponse::json(200, &completion),
                                    url: NEW_THREAD_URL.to_string(),
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
                                    url: NEW_THREAD_URL.to_string(),
                                    headers: HeaderList::new(),
                                    transformed_body: req.body,
                                    response_format: None,
                                });
                            }
                        }
                    }
                    Err(e) => {
                        let err = json!({
                            "error": { "message": format!("failed to open muse.ai page: {e}"), "type": "browser_error" }
                        });
                        return Ok(ExecResult {
                            response: UpstreamResponse::json(500, &err),
                            url: NEW_THREAD_URL.to_string(),
                            headers: HeaderList::new(),
                            transformed_body: req.body,
                            response_format: None,
                        });
                    }
                }
            }
            Err(e) => {
                // Browser pool unavailable (e.g. no Chromium installed) -> actionable error
                let (status, msg) = if crate::browser_executable_check::is_missing_browser_executable(&e.to_string()) {
                    (503, "Muse Web needs a Chrome/Chromium binary. Install Google Chrome, set NINEROUTER_BROWSER_BIN or CHROME_PATH, or install Chromium.".to_string())
                } else {
                    (500, format!("muse-web browser automation error: {e}"))
                };

                let err = json!({
                    "error": { "message": msg, "type": "internal_error" }
                });
                Ok(ExecResult {
                    response: UpstreamResponse::json(status, &err),
                    url: SITE.to_string(),
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
    Arc::new(MuseWebExecutor::new())
}

use sha2::Digest;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cookie_pairs() {
        let raw = "hatch_sess=abc123; hatch_gw=gw456; domain=.muse.ai";
        let pairs = parse_cookie_pairs(raw);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0, "hatch_sess");
        assert_eq!(pairs[0].1, "abc123");
        assert_eq!(pairs[1].0, "hatch_gw");
        assert_eq!(pairs[1].1, "gw456");
    }

    #[test]
    fn test_cookie_header_of() {
        let pairs = vec![("a".to_string(), "1".to_string()), ("b".to_string(), "2".to_string())];
        assert_eq!(cookie_header_of(&pairs), "a=1; b=2");
    }

    #[test]
    fn test_flatten_messages_single_user() {
        let msgs = json!([{"role": "user", "content": "Hello"}]);
        let (prompt, images) = flatten_messages(msgs.as_array().unwrap());
        assert_eq!(prompt, "Hello");
        assert!(images.is_empty());
    }

    #[test]
    fn test_flatten_messages_multi_turn() {
        let msgs = json!([
            {"role": "user", "content": "Hi"},
            {"role": "assistant", "content": "Hello"},
            {"role": "user", "content": "Help me"}
        ]);
        let (prompt, _) = flatten_messages(msgs.as_array().unwrap());
        assert!(prompt.contains("[User]\nHi"));
        assert!(prompt.contains("[Assistant]\nHello"));
        assert!(prompt.contains("[Instruction]"));
    }
}
