//! Port of open-sse/executors/deepseek-web.js — DeepSeek web chat executor.
//!
//! Handles Bearer userToken auth, token caching, PoW challenges, session management,
//! full SSE stream transformation (thinking content, web search citations, done terminator),
//! and tool call translation with repair loops.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;
use futures::{StreamExt, TryStreamExt};
use nr_common::sse::sse_chunk;
use nr_core::http::{proxy_fetch, BodyStream, FetchRequest, HeaderList, UpstreamResponse};
use nr_core::{Credentials, Error, ExecRequest, ExecResult, Executor, Json, Result};
use rand::Rng;
use serde_json::json;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::deepseek_utils::{
    DEEPSEEK_FINISHED_DRAIN_MS,
    append_search_citations, format_pow_response, format_stream_content, is_search_model,
    is_thinking_model, request_deepseek_pow_challenge, solve_deepseek_pow,
};
use crate::chatgpt_web_tools::sse_headers;
use crate::web_tools::{
    build_tool_conversation_prompt, has_malformed_deepseek_tool_intent,
    has_malformed_deepseek_tool_markup, parse_deepseek_tool_calls, serialize_deepseek_tool_prompt,
    ToolCall,
};

pub const DEEPSEEK_WEB_BASE: &str = "https://chat.deepseek.com";
const DEEPSEEK_API_BASE: &str = "https://chat.deepseek.com/api";
const COMPLETION_URL: &str = "https://chat.deepseek.com/api/v0/chat/completion";

lazy_static::lazy_static! {
    static ref TOKEN_CACHE: Mutex<HashMap<String, (String, u64)>> = Mutex::new(HashMap::new());
    static ref SESSION_CACHE: Mutex<HashMap<String, (String, u64)>> = Mutex::new(HashMap::new());
}
const CACHE_MAX_SIZE: usize = 100;
const DEFAULT_AUTO_HISTORY_WINDOW: usize = 20;

fn fake_headers() -> HeaderList {
    let mut h = HeaderList::new();
    h.insert("Accept".into(), "*/*".into());
    h.insert("Accept-Language".into(), "en-US,en;q=0.9".into());
    h.insert("Origin".into(), DEEPSEEK_WEB_BASE.into());
    h.insert("Referer".into(), format!("{}/", DEEPSEEK_WEB_BASE));
    h.insert(
        "User-Agent".into(),
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36".into(),
    );
    h.insert("X-Client-Bundle-Id".into(), "com.deepseek.chat".into());
    h.insert("X-Client-Locale".into(), "en-US".into());
    h.insert("X-Client-Platform".into(), "web".into());
    h.insert("X-Client-Version".into(), "2.0.0".into());
    h
}

fn generate_fake_cookie() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let mut rng = rand::thread_rng();
    let hex: String = (0..18)
        .map(|_| format!("{:x}", rng.gen_range(0..16)))
        .collect();
    let uid = Uuid::new_v4().to_string();
    format!(
        "intercom-HWWAFSESTIME={}; HWWAFSESID={}; Hm_lvt_{}={}; _frid={}",
        ts,
        hex,
        uid,
        ts / 1000,
        Uuid::new_v4()
    )
}

pub fn extract_user_token(creds: &Credentials) -> Option<String> {
    let raw = creds.api_key().or_else(|| creds.access_token())?;
    let trimmed = raw.trim();
    if trimmed.starts_with('{') {
        if let Ok(parsed) = serde_json::from_str::<Json>(trimmed) {
            if let Some(val) = parsed.get("value").and_then(|v| v.as_str()) {
                return Some(val.to_string());
            }
        }
    }
    if !trimmed.is_empty() {
        Some(trimmed.to_string())
    } else {
        None
    }
}

async fn acquire_access_token(
    user_token: &str,
    proxy: Option<&nr_core::http::ProxyOptions>,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    if let Ok(cache) = TOKEN_CACHE.try_lock() {
        if let Some((token, expires_at)) = cache.get(user_token) {
            if *expires_at > now {
                return Ok(token.clone());
            }
        }
    }

    let mut headers = fake_headers();
    headers.insert("Authorization".into(), format!("Bearer {}", user_token));

    let fetch_req = FetchRequest {
        header_timeout: None,
        cancel: cancel.cloned(),
        direct_fetch: None,
        ..FetchRequest::get(format!("{}/v0/users/current", DEEPSEEK_API_BASE))
    };
    let mut req_with_headers = fetch_req;
    req_with_headers.headers = headers;

    let res = proxy_fetch(req_with_headers, proxy).await?;
    if res.status == 401 || res.status == 403 {
        return Err(Error::other("Token invalid or expired — get a new userToken from DeepSeek localStorage"));
    }
    if !res.ok() {
        return Err(Error::other(format!("users/current HTTP {}", res.status)));
    }

    let json_val = res.json_body().await?;
    if let Some(code) = json_val.get("code").and_then(|c| c.as_i64()) {
        if code != 0 {
            let msg = json_val.get("msg").and_then(|m| m.as_str()).unwrap_or("error code");
            return Err(Error::other(format!("DeepSeek rejected token: {}", msg)));
        }
    }

    let biz_data = json_val
        .get("data")
        .and_then(|d| d.get("biz_data"))
        .or_else(|| json_val.get("biz_data"));
    let token = biz_data
        .and_then(|b| b.get("token"))
        .and_then(|t| t.as_str())
        .ok_or_else(|| Error::other("Failed to acquire token from DeepSeek"))?
        .to_string();

    let mut cache = TOKEN_CACHE.lock().await;
    if cache.len() >= CACHE_MAX_SIZE {
        if let Some(first_key) = cache.keys().next().cloned() {
            cache.remove(&first_key);
        }
    }
    cache.insert(user_token.to_string(), (token.clone(), now + 3600));

    Ok(token)
}

async fn create_session(
    access_token: &str,
    proxy: Option<&nr_core::http::ProxyOptions>,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<String> {
    let mut headers = fake_headers();
    headers.insert("Content-Type".into(), "application/json".into());
    headers.insert("Authorization".into(), format!("Bearer {}", access_token));
    headers.insert("Cookie".into(), generate_fake_cookie());

    let fetch_req = FetchRequest {
        header_timeout: None,
        cancel: cancel.cloned(),
        direct_fetch: None,
        ..FetchRequest::post_json(
            format!("{}/v0/chat_session/create", DEEPSEEK_API_BASE),
            headers,
            &json!({}),
        )
    };

    let res = proxy_fetch(fetch_req, proxy).await?;
    if !res.ok() {
        return Err(Error::other(format!("chat_session/create HTTP {}", res.status)));
    }

    let json_val = res.json_body().await?;
    let biz_data = json_val
        .get("data")
        .and_then(|d| d.get("biz_data"))
        .or_else(|| json_val.get("biz_data"));
    let id = biz_data
        .and_then(|b| b.get("chat_session"))
        .and_then(|cs| cs.get("id"))
        .and_then(|i| i.as_str())
        .ok_or_else(|| Error::other("No session id returned by DeepSeek"))?;

    Ok(id.to_string())
}

async fn delete_session_on_deepseek(
    access_token: &str,
    session_id: &str,
    proxy: Option<&nr_core::http::ProxyOptions>,
) {
    let mut headers = fake_headers();
    headers.insert("Content-Type".into(), "application/json".into());
    headers.insert("Authorization".into(), format!("Bearer {}", access_token));

    let body = json!({ "chat_session_id": session_id });
    let fetch_req = FetchRequest::post_json(
        format!("{}/v0/chat_session/delete", DEEPSEEK_API_BASE),
        headers,
        &body,
    );

    let _ = proxy_fetch(fetch_req, proxy).await;
}

fn resolve_model_options(model: &str, body: &Json) -> (String, bool, bool) {
    let m = model.to_lowercase();
    let model_type = if m.contains("pro") || m.contains("expert") {
        "expert".to_string()
    } else {
        "default".to_string()
    };

    let thinking_enabled = m.contains("r1")
        || m.contains("think")
        || m.contains("reason")
        || body.get("thinking_enabled").and_then(|v| v.as_bool()) == Some(true)
        || body.get("thinking").and_then(|v| v.as_bool()) == Some(true)
        || body.get("reasoning_effort").is_some();

    let search_enabled = m.contains("search")
        || body.get("search_enabled").and_then(|v| v.as_bool()) == Some(true)
        || body.get("search").and_then(|v| v.as_bool()) == Some(true)
        || body.get("web_search").and_then(|v| v.as_bool()) == Some(true);

    (model_type, thinking_enabled, search_enabled)
}

pub fn messages_to_prompt(messages: &[Json], history_window: usize) -> String {
    if messages.is_empty() {
        return String::new();
    }
    let mut system_parts = Vec::new();
    let mut conversation: Vec<(&str, String)> = Vec::new();
    let mut call_name_by_id: HashMap<String, String> = HashMap::new();
    let mut last_user_content = String::new();

    for m in messages {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let text = extract_message_text(m.get("content").unwrap_or(&Json::Null));

        if role == "system" {
            if !text.is_empty() {
                system_parts.push(text);
            }
        } else if role == "user" || role == "assistant" {
            if !text.is_empty() {
                if role == "user" {
                    last_user_content = text.clone();
                }
                conversation.push((role, text));
            }
            if let Some(calls) = m.get("tool_calls").and_then(|tc| tc.as_array()) {
                for c in calls {
                    if let (Some(id), Some(fn_obj)) = (
                        c.get("id").and_then(|i| i.as_str()),
                        c.get("function").and_then(|f| f.as_object()),
                    ) {
                        if let Some(name) = fn_obj.get("name").and_then(|n| n.as_str()) {
                            call_name_by_id.insert(id.to_string(), name.to_string());
                        }
                    }
                }
            }
        } else if role == "tool" && !text.is_empty() {
            let name = m
                .get("tool_call_id")
                .and_then(|id| id.as_str())
                .and_then(|id| call_name_by_id.get(id))
                .map(|s| s.as_str())
                .or_else(|| m.get("name").and_then(|n| n.as_str()))
                .unwrap_or("tool");
            conversation.push(("tool", format!("({}) {}", name, text)));
        }
    }

    let mut parts = Vec::new();
    if !system_parts.is_empty() {
        parts.push(system_parts.join("\n\n"));
    }

    let effective_window = if history_window > 0 {
        history_window
    } else if conversation.len() > 1 {
        DEFAULT_AUTO_HISTORY_WINDOW
    } else {
        0
    };

    if effective_window > 0 && conversation.len() > 1 {
        let start = conversation.len().saturating_sub(effective_window);
        let recent = &conversation[start..];
        let transcript = recent
            .iter()
            .map(|(role, text)| match *role {
                "assistant" => format!("Assistant: {}", text),
                "tool" => format!("Tool result {}", text),
                _ => format!("User: {}", text),
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        parts.push(transcript);
    } else if !last_user_content.is_empty() {
        parts.push(last_user_content);
    }

    parts.join("\n\n").replace("![", "[").replace("]", "")
}

fn extract_message_text(content: &Json) -> String {
    if let Some(arr) = content.as_array() {
        arr.iter()
            .filter(|item| item.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        content.as_str().unwrap_or("").to_string()
    }
}

// ── SSE Stream Transformation ───────────────────────────────────────────────

#[derive(Default)]
struct SseState {
    emitted_role: bool,
    current_path: String,
    saw_finished: bool,
    content: String,
    reasoning_content: String,
    search_results: Vec<Json>,
}

fn handle_deepseek_fragment(
    frag: &Json,
    set_path_from_type: bool,
    state: &mut SseState,
    stream_model: &str,
    chunks: &mut Vec<(Option<String>, Option<String>)>, // (content_delta, reasoning_delta)
) {
    let ftype = frag.get("type").and_then(|t| t.as_str()).unwrap_or("").to_uppercase();
    if set_path_from_type {
        if ftype == "THINK" {
            state.current_path = "thinking".to_string();
        } else if ftype == "ANSWER" || ftype == "RESPONSE" {
            state.current_path = "content".to_string();
        }
    }
    let content = match frag.get("content").and_then(|c| c.as_str()) {
        Some(c) if !c.is_empty() => c,
        _ => return,
    };
    if !set_path_from_type {
        if ftype == "THINK" {
            state.current_path = "thinking".to_string();
        } else if ftype == "ANSWER" || ftype == "RESPONSE" {
            state.current_path = "content".to_string();
        }
    }

    let text = format_stream_content(content, stream_model);
    if text.is_empty() {
        return;
    }

    let mut path = state.current_path.as_str();
    if path.is_empty() && is_thinking_model(stream_model) {
        path = "thinking";
    } else if path.is_empty() && is_search_model(stream_model) {
        path = "content";
    }

    if path == "thinking" {
        state.reasoning_content.push_str(&text);
        chunks.push((None, Some(text)));
    } else {
        state.content.push_str(&text);
        chunks.push((Some(text), None));
    }
}

fn process_deepseek_sse_payload(
    payload: &str,
    state: &mut SseState,
    stream_model: &str,
    chunks: &mut Vec<(Option<String>, Option<String>)>,
) {
    let data: Json = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return,
    };

    let p = data.get("p").and_then(|s| s.as_str()).unwrap_or("");
    let o = data.get("o").and_then(|s| s.as_str()).unwrap_or("");
    let v = data.get("v");

    if let Some(v_obj) = v.and_then(|val| val.as_object()) {
        if let Some(resp) = v_obj.get("response").and_then(|r| r.as_object()) {
            if resp.get("thinking_enabled") == Some(&Json::Bool(true)) {
                state.current_path = "thinking".to_string();
            } else if resp.get("thinking_enabled") == Some(&Json::Bool(false)) {
                state.current_path = "content".to_string();
            }
            if let Some(frags) = resp.get("fragments").and_then(|f| f.as_array()) {
                for frag in frags {
                    handle_deepseek_fragment(frag, false, state, stream_model, chunks);
                }
            }
        }
    }

    if p == "response/fragments" {
        if let Some(arr) = v.and_then(|val| val.as_array()) {
            for frag in arr {
                handle_deepseek_fragment(frag, true, state, stream_model, chunks);
            }
        } else if let Some(frag) = v {
            handle_deepseek_fragment(frag, true, state, stream_model, chunks);
        }
    }

    if p == "response" {
        if let Some(arr) = v.and_then(|val| val.as_array()) {
            for entry in arr {
                if entry.get("p").and_then(|s| s.as_str()) == Some("response")
                    && entry.get("v").and_then(|val| val.get("thinking_enabled")) == Some(&Json::Bool(true))
                {
                    state.current_path = "thinking".to_string();
                }
            }
        }
    }

    if p == "response/search_status" {
        return;
    }

    if p == "response/search_results" {
        if let Some(arr) = v.and_then(|val| val.as_array()) {
            if o != "BATCH" {
                state.search_results = arr.clone();
            } else {
                for op in arr {
                    let op_p = op.get("p").and_then(|s| s.as_str()).unwrap_or("");
                    if let Some(slash_idx) = op_p.find("/cite_index") {
                        if let Ok(idx) = op_p[..slash_idx].parse::<usize>() {
                            if idx < state.search_results.len() {
                                if let Some(cite_val) = op.get("v") {
                                    if let Some(target_obj) = state.search_results[idx].as_object_mut() {
                                        target_obj.insert("cite_index".to_string(), cite_val.clone());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        return;
    }

    if let Some(s) = v.and_then(|val| val.as_str()) {
        let text = format_stream_content(s, stream_model);
        if !text.is_empty() {
            let mut path = state.current_path.as_str();
            if path.is_empty() && is_thinking_model(stream_model) {
                path = "thinking";
            } else if path.is_empty() && is_search_model(stream_model) {
                path = "content";
            }
            if path == "thinking" {
                state.reasoning_content.push_str(&text);
                chunks.push((None, Some(text)));
            } else {
                state.content.push_str(&text);
                chunks.push((Some(text), None));
            }
        }
    } else if p == "response" {
        if let Some(arr) = v.and_then(|val| val.as_array()) {
            for entry in arr {
                if let Some(sub_arr) = entry.get("v").and_then(|val| val.as_array()) {
                    let joined = sub_arr
                        .iter()
                        .filter_map(|item| item.get("content").and_then(|c| c.as_str()))
                        .collect::<Vec<_>>()
                        .join("");
                    let text = format_stream_content(&joined, stream_model);
                    if !text.is_empty() {
                        let path = if state.current_path.is_empty() && is_thinking_model(stream_model) {
                            "thinking"
                        } else {
                            "content"
                        };
                        if path == "thinking" {
                            state.reasoning_content.push_str(&text);
                            chunks.push((None, Some(text)));
                        } else {
                            state.content.push_str(&text);
                            chunks.push((Some(text), None));
                        }
                    }
                }
            }
        }
    }

    if p == "response/status" && v.and_then(|val| val.as_str()) == Some("FINISHED") {
        state.saw_finished = true;
    }
}

async fn collect_sse_content(
    stream_response: UpstreamResponse,
    stream_model: &str,
) -> Result<(String, String)> {
    let mut state = SseState::default();
    let body_bytes = stream_response.bytes().await?;
    let text = String::from_utf8_lossy(&body_bytes);
    let mut dummy_chunks = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("data:") {
            continue;
        }
        let payload = trimmed.trim_start_matches("data:").trim();
        if payload == "[DONE]" {
            break;
        }
        process_deepseek_sse_payload(payload, &mut state, stream_model, &mut dummy_chunks);
    }

    let citations = append_search_citations(&state.search_results, stream_model);
    if !citations.is_empty() {
        if !state.content.is_empty() {
            state.content.push_str("\n\n");
        }
        state.content.push_str(&citations);
    }

    if !state.saw_finished {
        return Err(Error::other(
            "DeepSeek web session ended before completion (no FINISHED signal received) — likely a dropped session or network interruption upstream.",
        ));
    }

    Ok((state.content, state.reasoning_content))
}

fn build_tool_aware_openai_result(
    stream: bool,
    client_model: &str,
    content: &str,
    reasoning_content: &str,
    tool_calls: &[ToolCall],
    cid: &str,
    created: i64,
) -> UpstreamResponse {
    let has_calls = !tool_calls.is_empty();
    let finish_reason = if has_calls { "tool_calls" } else { "stop" };

    if !stream {
        let mut msg = json!({ "role": "assistant" });
        if !reasoning_content.is_empty() {
            msg["reasoning_content"] = Json::String(reasoning_content.to_string());
        }
        if has_calls {
            let tc_arr: Vec<Json> = tool_calls
                .iter()
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
            msg["tool_calls"] = Json::Array(tc_arr);
            msg["content"] = if content.is_empty() { Json::Null } else { Json::String(content.to_string()) };
        } else {
            msg["content"] = Json::String(content.to_string());
        }

        let body = json!({
            "id": cid,
            "object": "chat.completion",
            "created": created,
            "model": client_model,
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
        "model": client_model,
        "choices": [{ "index": 0, "delta": { "role": "assistant", "content": "" }, "finish_reason": Json::Null }]
    });
    output.push_str(&sse_chunk(&c1));

    if !reasoning_content.is_empty() {
        let c_r = json!({
            "id": cid,
            "object": "chat.completion.chunk",
            "created": created,
            "model": client_model,
            "choices": [{ "index": 0, "delta": { "reasoning_content": reasoning_content }, "finish_reason": Json::Null }]
        });
        output.push_str(&sse_chunk(&c_r));
    }

    if !content.is_empty() {
        let c_c = json!({
            "id": cid,
            "object": "chat.completion.chunk",
            "created": created,
            "model": client_model,
            "choices": [{ "index": 0, "delta": { "content": content }, "finish_reason": Json::Null }]
        });
        output.push_str(&sse_chunk(&c_c));
    }

    if has_calls {
        for (idx, tc) in tool_calls.iter().enumerate() {
            let c_tc = json!({
                "id": cid,
                "object": "chat.completion.chunk",
                "created": created,
                "model": client_model,
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
        "model": client_model,
        "choices": [{ "index": 0, "delta": {}, "finish_reason": finish_reason }]
    });
    output.push_str(&sse_chunk(&c_fin));
    output.push_str("data: [DONE]\n\n");

    let stream_bytes = Box::pin(futures::stream::once(async move {
        Ok::<Bytes, std::io::Error>(Bytes::from(output))
    }));
    UpstreamResponse::sse(stream_bytes)
}

fn transform_sse_stream(
    stream_response: UpstreamResponse,
    client_model: &str,
    cid: &str,
    created: i64,
) -> UpstreamResponse {
    let model = client_model.to_string();
    let cid_str = cid.to_string();
    let drain = std::time::Duration::from_millis(DEEPSEEK_FINISHED_DRAIN_MS);

    let body: BodyStream = Box::pin(async_stream::stream! {
        let mut state = SseState::default();
        // Drain window armed once `response/status=FINISHED` arrives: late
        // search_results still get captured, but the stream always closes so
        // clients do not hang on an upstream body that stays open.
        let mut drain_deadline: Option<tokio::time::Instant> = None;

        let mut body = stream_response.body;
        loop {
            let next = if let Some(deadline) = drain_deadline {
                match tokio::time::timeout_at(deadline, body.next()).await {
                    Ok(item) => item,
                    Err(_elapsed) => None,
                }
            } else {
                body.next().await
            };

            let Some(item) = next else {
                // Drain elapsed (or upstream body closed): always emit the
                // terminator, then end the stream.
                break;
            };
            let Ok(bytes) = item else {
                break;
            };
            let text = String::from_utf8_lossy(&bytes);
            let mut out = String::new();
            let mut chunks = Vec::new();

            for line in text.lines() {
                let trimmed = line.trim();
                if !trimmed.starts_with("data:") {
                    continue;
                }
                let payload = trimmed.trim_start_matches("data:").trim();
                if payload == "[DONE]" {
                    out.push_str("data: [DONE]\n\n");
                    continue;
                }
                chunks.clear();
                process_deepseek_sse_payload(payload, &mut state, &model, &mut chunks);

                for (content_delta, reasoning_delta) in chunks.drain(..) {
                    if !state.emitted_role {
                        let role_chunk = json!({
                            "id": cid_str,
                            "object": "chat.completion.chunk",
                            "created": created,
                            "model": model,
                            "choices": [{ "index": 0, "delta": { "role": "assistant", "content": "" }, "finish_reason": Json::Null }]
                        });
                        out.push_str(&sse_chunk(&role_chunk));
                        state.emitted_role = true;
                    }
                    if let Some(r) = reasoning_delta {
                        let c = json!({
                            "id": cid_str,
                            "object": "chat.completion.chunk",
                            "created": created,
                            "model": model,
                            "choices": [{ "index": 0, "delta": { "reasoning_content": r }, "finish_reason": Json::Null }]
                        });
                        out.push_str(&sse_chunk(&c));
                    }
                    if let Some(c_text) = content_delta {
                        let c = json!({
                            "id": cid_str,
                            "object": "chat.completion.chunk",
                            "created": created,
                            "model": model,
                            "choices": [{ "index": 0, "delta": { "content": c_text }, "finish_reason": Json::Null }]
                        });
                        out.push_str(&sse_chunk(&c));
                    }
                }

                if state.saw_finished {
                    // Drain (re)starts on FINISHED and on every later payload,
                    // so trailing search_results extend the window.
                    drain_deadline = Some(tokio::time::Instant::now() + drain);
                }
            }

            if !out.is_empty() {
                yield Ok(Bytes::from(out));
            }
        }

        if !state.emitted_role {
            let role_chunk = json!({
                "id": cid_str,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{ "index": 0, "delta": { "role": "assistant", "content": "" }, "finish_reason": Json::Null }]
            });
            yield Ok(Bytes::from(sse_chunk(&role_chunk)));
        }
        let citations = append_search_citations(&state.search_results, &model);
        if !citations.is_empty() {
            let c = json!({
                "id": cid_str,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{ "index": 0, "delta": { "content": format!("\n\n{}", citations) }, "finish_reason": Json::Null }]
            });
            yield Ok(Bytes::from(sse_chunk(&c)));
        }
        let fin = json!({
            "id": cid_str,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }]
        });
        yield Ok(Bytes::from(sse_chunk(&fin)));
        yield Ok(Bytes::from("data: [DONE]\n\n"));
    });

    UpstreamResponse::from_stream(200, sse_headers(), body)
}

// ── Executor Implementation ────────────────────────────────────────────────

pub struct DeepSeekWebExecutor;

#[async_trait]
impl Executor for DeepSeekWebExecutor {
    fn provider(&self) -> &str {
        "deepseek-web"
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        let user_token = extract_user_token(req.credentials).ok_or_else(|| {
            Error::BadRequest(
                "Missing userToken — paste it from DeepSeek localStorage (DevTools → Application → Local Storage → chat.deepseek.com → userToken)".into()
            )
        })?;

        let body_obj = &req.body;
        let requested_tools = body_obj.get("tools").and_then(|t| t.as_array());
        let has_tools = requested_tools.map(|t| !t.is_empty()).unwrap_or(false);
        let tool_system_prompt = if has_tools {
            serialize_deepseek_tool_prompt(requested_tools.unwrap())
        } else {
            String::new()
        };

        let empty_vec = Vec::new();
        let messages = body_obj
            .get("messages")
            .and_then(|m| m.as_array())
            .unwrap_or(&empty_vec);

        let persist_session = req.credentials.psd_str("persistSession") == Some("true");
        let history_window = req
            .credentials
            .psd_str("historyWindow")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);

        let access_token = acquire_access_token(&user_token, Some(&req.proxy), Some(&req.cancel)).await?;

        let prompt = if has_tools {
            build_tool_conversation_prompt(messages, &tool_system_prompt)
        } else if !tool_system_prompt.is_empty() {
            let mut prompt_msgs = vec![json!({"role": "system", "content": tool_system_prompt})];
            prompt_msgs.extend(messages.clone());
            messages_to_prompt(&prompt_msgs, history_window)
        } else {
            messages_to_prompt(messages, history_window)
        };

        let (model_type, thinking_enabled, search_enabled) =
            resolve_model_options(req.model, body_obj);

        let client_model = if !req.model.trim().is_empty() {
            req.model.trim().to_string()
        } else {
            "deepseek-web".to_string()
        };

        // Helper to perform a completion attempt with PoW challenge
        let perform_completion = |session_id: &str, comp_prompt: &str| {
            let token = access_token.clone();
            let sid = session_id.to_string();
            let pr = comp_prompt.to_string();
            let mt = model_type.clone();
            let proxy_opts = req.proxy.clone();
            let cancel_token = req.cancel.clone();
            let ref_files = body_obj.get("ref_file_ids").cloned().unwrap_or_else(|| json!([]));

            async move {
                let pow_challenge = request_deepseek_pow_challenge(
                    &token,
                    &fake_headers(),
                    Some(&proxy_opts),
                    Some(&cancel_token),
                    "/api/v0/chat/completion",
                )
                .await?;
                let pow_answer = solve_deepseek_pow(&pow_challenge)?;
                let pow_resp = format_pow_response(&pow_challenge, pow_answer);

                let mut headers = fake_headers();
                headers.insert("Content-Type".into(), "application/json".into());
                headers.insert("Authorization".into(), format!("Bearer {}", token));
                headers.insert("X-Ds-Pow-Response".into(), pow_resp);
                headers.insert("X-Client-Timezone-Offset".into(), "0".into());
                headers.insert("Cookie".into(), generate_fake_cookie());

                let payload = json!({
                    "chat_session_id": sid,
                    "parent_message_id": Json::Null,
                    "model_type": mt,
                    "prompt": pr,
                    "ref_file_ids": ref_files,
                    "thinking_enabled": thinking_enabled,
                    "search_enabled": search_enabled,
                    "preempt": false,
                });

                let fetch_req = FetchRequest {
                    header_timeout: None,
                    cancel: Some(cancel_token),
                    direct_fetch: None,
                    ..FetchRequest::post_json(COMPLETION_URL, headers.clone(), &payload)
                };

                let resp = proxy_fetch(fetch_req, Some(&proxy_opts)).await?;
                Ok::<(UpstreamResponse, HeaderList, Json), Error>((resp, headers, payload))
            }
        };

        // Acquire session
        let (mut session_id, mut reused_session) = if persist_session {
            let mut cached = None;
            {
                let cache = SESSION_CACHE.lock().await;
                if let Some((id, _)) = cache.get(&user_token) {
                    cached = Some(id.clone());
                }
            }
            if let Some(id) = cached {
                (id, true)
            } else {
                let id = create_session(&access_token, Some(&req.proxy), Some(&req.cancel)).await?;
                SESSION_CACHE.lock().await.insert(user_token.clone(), (id.clone(), 0));
                (id, false)
            }
        } else {
            let id = create_session(&access_token, Some(&req.proxy), Some(&req.cancel)).await?;
            (id, false)
        };

        let mut comp_result = perform_completion(&session_id, &prompt).await;

        // If reused session failed, retry once with a fresh session
        if let Ok((ref resp, _, _)) = comp_result {
            if !resp.ok() && persist_session && reused_session {
                SESSION_CACHE.lock().await.remove(&user_token);
                session_id = create_session(&access_token, Some(&req.proxy), Some(&req.cancel)).await?;
                reused_session = false;
                comp_result = perform_completion(&session_id, &prompt).await;
            }
        }

        let (resp, req_headers, request_payload) = comp_result?;

        if !resp.ok() {
            let status = resp.status;
            let err_msg = match status {
                401 | 403 => {
                    TOKEN_CACHE.lock().await.remove(&user_token);
                    "DeepSeek token expired — get a fresh userToken from localStorage.".to_string()
                }
                429 => "DeepSeek rate limited. Wait and retry.".to_string(),
                _ => format!("DeepSeek API error ({})", status),
            };

            if persist_session {
                SESSION_CACHE.lock().await.remove(&user_token);
            }
            delete_session_on_deepseek(&access_token, &session_id, Some(&req.proxy)).await;

            return Ok(ExecResult {
                response: UpstreamResponse::json(status, &json!({ "error": { "message": err_msg } })),
                url: COMPLETION_URL.to_string(),
                headers: req_headers,
                transformed_body: request_payload,
                response_format: None,
            });
        }

        let cid = format!("chatcmpl-{}", Uuid::new_v4());
        let created = Utc::now().timestamp();

        // Tool-call translation branch
        if has_tools {
            let (mut content, mut reasoning_content) =
                collect_sse_content(resp, &client_model).await?;
            let tools_slice = requested_tools.unwrap();
            let mut parsed = parse_deepseek_tool_calls(&content, &format!("call-{}", created), tools_slice);

            // Repair malformed tool intent once in fresh session
            if has_malformed_deepseek_tool_intent(&parsed.content, tools_slice) {
                delete_session_on_deepseek(&access_token, &session_id, Some(&req.proxy)).await;
                session_id = create_session(&access_token, Some(&req.proxy), Some(&req.cancel)).await?;
                let repair_prompt = format!(
                    "{}\n\nYour previous response used an invalid tool envelope and was rejected.\n\nRetry the requested next action from scratch. Follow the client tool contract exactly, including its _nonce binding and argument schema. Do not emit bare JSON or unbound DSML.",
                    prompt
                );
                let (repair_resp, _, _) = perform_completion(&session_id, &repair_prompt).await?;
                if repair_resp.ok() {
                    let (rep_content, rep_reasoning) =
                        collect_sse_content(repair_resp, &client_model).await?;
                    let rep_parsed = parse_deepseek_tool_calls(
                        &rep_content,
                        &format!("call-{}", created),
                        tools_slice,
                    );
                    if !has_malformed_deepseek_tool_markup(&rep_parsed.content) {
                        content = rep_content;
                        reasoning_content = rep_reasoning;
                        parsed = rep_parsed;
                    }
                }
            }

            if !persist_session {
                delete_session_on_deepseek(&access_token, &session_id, Some(&req.proxy)).await;
            }

            let response = build_tool_aware_openai_result(
                req.stream,
                &client_model,
                &parsed.content,
                &reasoning_content,
                &parsed.tool_calls,
                &cid,
                created,
            );

            return Ok(ExecResult {
                response,
                url: COMPLETION_URL.to_string(),
                headers: req_headers,
                transformed_body: request_payload,
                response_format: None,
            });
        }

        // Standard chat: streaming or non-streaming
        let response = if req.stream {
            transform_sse_stream(resp, &client_model, &cid, created)
        } else {
            let (content, reasoning_content) = collect_sse_content(resp, &client_model).await?;
            if !persist_session {
                delete_session_on_deepseek(&access_token, &session_id, Some(&req.proxy)).await;
            }
            let mut msg = json!({ "role": "assistant", "content": content });
            if !reasoning_content.is_empty() {
                msg["reasoning_content"] = Json::String(reasoning_content);
            }
            let body = json!({
                "id": cid,
                "object": "chat.completion",
                "created": created,
                "model": client_model,
                "choices": [{
                    "index": 0,
                    "message": msg,
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 }
            });
            UpstreamResponse::json(200, &body)
        };

        Ok(ExecResult {
            response,
            url: COMPLETION_URL.to_string(),
            headers: req_headers,
            transformed_body: request_payload,
            response_format: None,
        })
    }
}

/// Create the executor instance
pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(DeepSeekWebExecutor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderMap;

    #[test]
    fn test_extract_user_token_raw() {
        let mut creds = Credentials::new();
        creds.set("apiKey", "raw-token-string");
        assert_eq!(extract_user_token(&creds), Some("raw-token-string".to_string()));
    }

    #[test]
    fn test_extract_user_token_json_wrapped() {
        let mut creds = Credentials::new();
        creds.set("apiKey", r#"{"value":"json-wrapped-token"}"#);
        assert_eq!(extract_user_token(&creds), Some("json-wrapped-token".to_string()));
    }

    #[test]
    fn test_extract_user_token_with_access_token() {
        let mut creds = Credentials::new();
        creds.set("accessToken", "access-token-value");
        assert_eq!(extract_user_token(&creds), Some("access-token-value".to_string()));
    }

    #[test]
    fn test_extract_user_token_empty() {
        let creds = Credentials::new();
        assert_eq!(extract_user_token(&creds), None);
    }

    #[test]
    fn test_messages_to_prompt_single_user() {
        let messages = vec![json!({"role": "user", "content": "Hello"})];
        let prompt = messages_to_prompt(&messages, 0);
        assert!(prompt.contains("Hello"));
    }

    #[test]
    fn test_messages_to_prompt_with_system() {
        let messages = vec![
            json!({"role": "system", "content": "System message"}),
            json!({"role": "user", "content": "User message"}),
        ];
        let prompt = messages_to_prompt(&messages, 0);
        assert!(prompt.contains("System message"));
        assert!(prompt.contains("User message"));
    }

    #[test]
    fn test_provider_name() {
        let exec = DeepSeekWebExecutor;
        assert_eq!(exec.provider(), "deepseek-web");
    }

    /// A `FINISHED` status with an upstream body that stays open must still
    /// emit exactly one terminal `stop` + `[DONE]` after the drain window —
    /// this is what prevents OpenAI SDK clients hanging forever.
    #[tokio::test]
    async fn test_transform_sse_stream_closes_after_drain() {
        let body: BodyStream = Box::pin(async_stream::stream! {
            // Emulate an upstream that never closes: the drain must end us.
            yield Ok::<Bytes, std::io::Error>(Bytes::from(
                "data: {\"p\":\"response/status\",\"v\":\"FINISHED\"}\n\n",
            ));
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        });
        let upstream = UpstreamResponse::from_stream(200, HeaderMap::new(), body);

        let out = transform_sse_stream(upstream, "deepseek-chat", "cid", 1)
            .text()
            .await
            .unwrap();

        assert!(out.ends_with("data: [DONE]\n\n"), "stream must terminate: {out}");
        assert_eq!(out.matches("data: [DONE]").count(), 1, "single terminator: {out}");
        assert_eq!(out.matches("\"finish_reason\":\"stop\"").count(), 1, "{out}");
        assert!(out.contains("\"role\":\"assistant\""), "role chunk emitted: {out}");
    }
}
