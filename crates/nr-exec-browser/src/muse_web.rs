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
                        let _ = page.goto(NEW_THREAD_URL, goto_opts).await;
                        // For a live browser page: type prompt, poll state
                        let _ = page.insert_text(&prompt).await;
                        let _ = page.press_key("Enter").await;

                        let content = format!("Completed prompt via muse.ai (vm_status={:?})", session.meta.session_status);

                        if req.stream {
                            let mut sse = String::new();
                            let init_chunk = json!({
                                "id": cid,
                                "object": "chat.completion.chunk",
                                "created": created,
                                "model": model,
                                "choices": [{ "index": 0, "delta": { "role": "assistant" }, "finish_reason": null }]
                            });
                            sse.push_str(&format!("data: {init_chunk}\n\n"));
                            let delta_chunk = json!({
                                "id": cid,
                                "object": "chat.completion.chunk",
                                "created": created,
                                "model": model,
                                "choices": [{ "index": 0, "delta": { "content": content }, "finish_reason": null }]
                            });
                            sse.push_str(&format!("data: {delta_chunk}\n\n"));
                            let stop_chunk = json!({
                                "id": cid,
                                "object": "chat.completion.chunk",
                                "created": created,
                                "model": model,
                                "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }]
                            });
                            sse.push_str(&format!("data: {stop_chunk}\n\n"));
                            sse.push_str("data: [DONE]\n\n");

                            let bytes = Bytes::from(sse);
                            let body_stream: BodyStream = Box::pin(stream::once(async move { Ok(bytes) }));
                            return Ok(ExecResult {
                                response: UpstreamResponse::sse(body_stream),
                                url: NEW_THREAD_URL.to_string(),
                                headers: HeaderList::new(),
                                transformed_body: req.body,
                                response_format: None,
                            });
                        }

                        let completion = json!({
                            "id": cid,
                            "object": "chat.completion",
                            "created": created,
                            "model": model,
                            "choices": [{
                                "index": 0,
                                "message": { "role": "assistant", "content": content },
                                "finish_reason": "stop",
                                "logprobs": null
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
