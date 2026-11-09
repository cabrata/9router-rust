//! Port of open-sse/executors/perplexity-web.js — Perplexity web chat executor.
//!
//! Handles web search API with streaming response, session caching, and model mapping.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;
use futures::{Stream, StreamExt};
use http::HeaderMap;
use lazy_static::lazy_static;
use nr_common::sse::sse_chunk;
use nr_core::http::{proxy_fetch, FetchRequest, UpstreamResponse};
use nr_core::{ExecRequest, ExecResult, Executor, Result};
use regex::Regex;
use serde_json::{json, Value};
use uuid::Uuid;

const PPLX_SSE_ENDPOINT: &str = "https://api.perplexity.ai/chat/send_message";
const PPLX_API_VERSION: &str = "2.18";
const PPLX_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

const SESSION_MAX_AGE_MS: u64 = 3600_000;
const SESSION_MAX_ENTRIES: usize = 200;
const POLL_INTERVAL_MS: u64 = 5000;

lazy_static::lazy_static! {
    static ref MODEL_MAP: HashMap<&'static str, (&'static str, &'static str)> = {
        let mut m = HashMap::new();
        m.insert("pplx-auto", ("concise", "pplx_pro"));
        m.insert("pplx-sonar", ("copilot", "experimental"));
        m.insert("pplx-gpt", ("copilot", "gpt54"));
        m.insert("pplx-gemini", ("copilot", "gemini31pro_high"));
        m.insert("pplx-sonnet", ("copilot", "claude46sonnet"));
        m.insert("pplx-opus", ("copilot", "claude46opus"));
        m.insert("pplx-nemotron", ("copilot", "nv_nemotron_3_super"));
        m
    };

    static ref THINKING_MAP: HashMap<&'static str, &'static str> = {
        let mut m = HashMap::new();
        m.insert("pplx-gpt", "gpt54_thinking");
        m.insert("pplx-sonnet", "claude46sonnetthinking");
        m.insert("pplx-opus", "claude46opusthinking");
        m
    };

    static ref CITATION_RE: Regex = Regex::new(r"\[\d+\]").unwrap();
    static ref GROK_TAG_RE: Regex = Regex::new(r"<grok:[^>]*>.*?</grok:[^>]*>").unwrap();
    static ref GROK_SELF_RE: Regex = Regex::new(r"<grok:[^>]*/?>").unwrap();
    static ref XML_DECL_RE: Regex = Regex::new(r"<\?xml[^?]*\?>").unwrap();
    static ref RESPONSE_TAG_RE: Regex = Regex::new(r"</?\s*response\b[^>]*>").unwrap();

    static ref SESSION_CACHE: Mutex<HashMap<String, SessionEntry>> = Mutex::new(HashMap::new());
}

#[derive(Clone)]
struct SessionEntry {
    backend_uuid: String,
    ts: u64,
}

fn session_key(history: &[Value], current_msg: &str) -> String {
    let mut parts = String::new();
    for h in history {
        if let (Some(role), Some(content)) = (h.get("role").and_then(|r| r.as_str()), h.get("content").and_then(|c| c.as_str())) {
            parts.push_str(&format!("{}:{}\n", role, content));
        }
    }
    parts.push_str(&format!("user:{}\n", current_msg));

    let mut hash: u32 = 0x811c9dc5;
    for byte in parts.as_bytes() {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(0x01000193);
    }
    format!("{:08x}", hash)
}

fn session_lookup(history: &[Value], current_msg: &str) -> Option<String> {
    let key = session_key(history, current_msg);
    let cache = SESSION_CACHE.lock().ok()?;
    let entry = cache.get(&key)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_millis() as u64;
    if now - entry.ts > SESSION_MAX_AGE_MS {
        return None;
    }
    Some(entry.backend_uuid.clone())
}

fn session_store(history: &[Value], current_msg: &str, response_text: &str, backend_uuid: &str) {
    if backend_uuid.is_empty() {
        return;
    }
    let mut full_history = history.to_vec();
    full_history.push(json!({ "role": "user", "content": current_msg }));
    full_history.push(json!({ "role": "assistant", "content": response_text }));

    let key = session_key(&full_history, current_msg);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_millis() as u64).unwrap_or(0);

    if let Ok(mut cache) = SESSION_CACHE.lock() {
        cache.insert(key, SessionEntry { backend_uuid: backend_uuid.to_string(), ts: now });
        if cache.len() > SESSION_MAX_ENTRIES {
            if let Some((oldest_key, _)) = cache.iter().min_by_key(|(_, entry)| entry.ts).map(|(k, v)| (k.clone(), v.clone())) {
                cache.remove(&oldest_key);
            }
        }
    }
}

fn clean_response(text: &str, strip: bool) -> String {
    let mut t = text.to_string();
    t = XML_DECL_RE.replace_all(&t, "").to_string();
    t = CITATION_RE.replace_all(&t, "").to_string();
    t = GROK_TAG_RE.replace_all(&t, "").to_string();
    t = GROK_SELF_RE.replace_all(&t, "").to_string();
    t = RESPONSE_TAG_RE.replace_all(&t, "").to_string();

    if strip {
        t = Regex::new(r" {2,}").unwrap_or_else(|_| Regex::new(" ").unwrap()).replace_all(&t, " ").to_string();
        t = Regex::new(r"\n{3,}").unwrap_or_else(|_| Regex::new("\n").unwrap()).replace_all(&t, "\n\n").to_string();
        t = t.trim().to_string();
    }
    t
}

#[derive(Clone, Debug)]
struct ParsedMessages {
    system_msg: String,
    history: Vec<Value>,
    current_msg: String,
}

fn parse_openai_messages(messages: &[Value]) -> ParsedMessages {
    let mut system_msg = String::new();
    let mut history = Vec::new();

    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let mut content = String::new();

        if let Some(str_content) = msg.get("content").and_then(|c| c.as_str()) {
            content = str_content.to_string();
        } else if let Some(arr) = msg.get("content").and_then(|c| c.as_array()) {
            let text_parts: Vec<String> = arr
                .iter()
                .filter(|item| item.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|item| item.get("text").and_then(|t| t.as_str()).map(String::from))
                .collect();
            content = text_parts.join(" ");
        }

        if content.trim().is_empty() {
            continue;
        }

        let normalized_role = if role == "developer" { "system" } else { role };

        if normalized_role == "system" {
            system_msg.push_str(&content);
            system_msg.push('\n');
        } else if normalized_role == "user" || normalized_role == "assistant" {
            history.push(json!({ "role": normalized_role, "content": content }));
        }
    }

    let mut current_msg = String::new();
    if !history.is_empty() && history[history.len() - 1].get("role").and_then(|r| r.as_str()) == Some("user") {
        current_msg = history
            .pop()
            .and_then(|msg| msg.get("content").and_then(|c| c.as_str()).map(String::from))
            .unwrap_or_default();
    }

    ParsedMessages { system_msg, history, current_msg }
}

fn format_tools_hint(tools: Option<&[Value]>) -> String {
    if let Some(tools) = tools {
        if tools.is_empty() {
            return String::new();
        }

        let mut lines = Vec::new();
        for tool in tools {
            let fn_obj = tool.get("function").unwrap_or(tool);
            let name = fn_obj.get("name").and_then(|n| n.as_str()).unwrap_or("unnamed");
            let desc = fn_obj
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(200)
                .collect::<String>();
            lines.push(format!("- {}: {}", name, desc));
        }

        format!("Available tools (reference only, cannot invoke):\n{}", lines.join("\n"))
    } else {
        String::new()
    }
}

fn build_query(parsed: &ParsedMessages, follow_up_uuid: Option<&str>, tools: Option<&[Value]>) -> String {
    if follow_up_uuid.is_some() {
        return parsed.current_msg.clone();
    }

    let mut instr = Vec::new();
    if !parsed.system_msg.trim().is_empty() {
        instr.push(parsed.system_msg.trim().to_string());
    }

    let tools_hint = format_tools_hint(tools);
    if !tools_hint.is_empty() {
        instr.push(tools_hint);
    }

    instr.push("You have built-in web search. Answer questions directly using search results.".to_string());

    let mut obj = json!({
        "instructions": instr,
    });

    if !parsed.history.is_empty() {
        obj["history"] = json!(parsed.history);
    }
    if !parsed.current_msg.is_empty() {
        obj["query"] = json!(parsed.current_msg);
    } else if parsed.history.is_empty() {
        obj["query"] = json!("");
    }

    let json_str = obj.to_string();
    if json_str.len() > 96000 {
        json_str[json_str.len() - 96000..].to_string()
    } else {
        json_str
    }
}

fn build_pplx_request_body(query: &str, mode: &str, model_pref: &str, follow_up_uuid: Option<&str>) -> Value {
    let tz = "UTC";
    json!({
        "query_str": query,
        "params": {
            "query_str": query,
            "search_focus": "internet",
            "mode": mode,
            "model_preference": model_pref,
            "sources": ["web"],
            "attachments": [],
            "frontend_uuid": Uuid::new_v4().to_string(),
            "frontend_context_uuid": Uuid::new_v4().to_string(),
            "version": PPLX_API_VERSION,
            "language": "en-US",
            "timezone": tz,
            "search_recency_filter": Value::Null,
            "is_incognito": true,
            "use_schematized_api": true,
            "last_backend_uuid": follow_up_uuid.unwrap_or_default(),
        },
    })
}

async fn read_pplx_sse_events(body: bytes::Bytes) -> Result<Vec<Value>> {
    let text = String::from_utf8_lossy(&body);
    let mut events = Vec::new();
    let mut data_lines = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim_end_matches('\r');

        if trimmed.is_empty() {
            if !data_lines.is_empty() {
                let payload = data_lines.join("\n");
                data_lines.clear();
                let trimmed_payload = payload.trim();
                if trimmed_payload == "[DONE]" || trimmed_payload.is_empty() {
                    continue;
                }
                if let Ok(parsed) = serde_json::from_str::<Value>(trimmed_payload) {
                    events.push(parsed);
                }
            }
        } else if trimmed.starts_with("data:") {
            data_lines.push(trimmed[5..].trim_start().to_string());
        } else if trimmed == "event: end_of_stream" {
            break;
        }
    }

    Ok(events)
}

#[derive(Debug, Clone)]
enum ContentChunk {
    Thinking { text: String, backend_uuid: Option<String> },
    Delta { text: String, answer: String, backend_uuid: Option<String> },
    Done { answer: String, backend_uuid: Option<String> },
    Error { message: String },
}

async fn extract_content(events: &[Value]) -> Result<Vec<ContentChunk>> {
    let mut chunks = Vec::new();
    let mut full_answer = String::new();
    let mut backend_uuid: Option<String> = None;
    let mut seen_len = 0;
    let mut seen_thinking = std::collections::HashSet::new();

    for event in events {
        if let Some(error_msg) = event.get("error_message").and_then(|e| e.as_str()) {
            chunks.push(ContentChunk::Error { message: error_msg.to_string() });
            return Ok(chunks);
        }

        if let Some(uuid) = event.get("backend_uuid").and_then(|u| u.as_str()) {
            backend_uuid = Some(uuid.to_string());
        }

        if let Some(blocks) = event.get("blocks").and_then(|b| b.as_array()) {
            for block in blocks {
                let usage = block.get("intended_usage").and_then(|u| u.as_str()).unwrap_or("");

                if usage == "pro_search_steps" {
                    if let Some(plan_block) = block.get("plan_block") {
                        if let Some(steps) = plan_block.get("steps").and_then(|s| s.as_array()) {
                            for step in steps {
                                if let Some("SEARCH_WEB") = step.get("step_type").and_then(|t| t.as_str()) {
                                    if let Some(queries) = step.get("search_web_content").and_then(|s| s.get("queries")).and_then(|q| q.as_array()) {
                                        for q in queries {
                                            if let Some(query_str) = q.get("query").and_then(|s| s.as_str()) {
                                                if seen_thinking.insert(query_str.to_string()) {
                                                    chunks.push(ContentChunk::Thinking {
                                                        text: format!("Searching: {}", query_str),
                                                        backend_uuid: backend_uuid.clone(),
                                                    });
                                                }
                                            }
                                        }
                                    }
                                } else if let Some("READ_RESULTS") = step.get("step_type").and_then(|t| t.as_str()) {
                                    if let Some(urls) = step.get("read_results_content").and_then(|r| r.get("urls")).and_then(|u| u.as_array()) {
                                        for url_item in urls.iter().take(3) {
                                            if let Some(url_str) = url_item.as_str() {
                                                if seen_thinking.insert(url_str.to_string()) {
                                                    chunks.push(ContentChunk::Thinking {
                                                        text: format!("Reading: {}", url_str),
                                                        backend_uuid: backend_uuid.clone(),
                                                    });
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                if usage == "plan" {
                    if let Some(plan_block) = block.get("plan_block") {
                        if let Some(goals) = plan_block.get("goals").and_then(|g| g.as_array()) {
                            for goal in goals {
                                if let Some(desc) = goal.get("description").and_then(|d| d.as_str()) {
                                    if !desc.is_empty() && seen_thinking.insert(desc.to_string()) {
                                        chunks.push(ContentChunk::Thinking {
                                            text: desc.to_string(),
                                            backend_uuid: backend_uuid.clone(),
                                        });
                                    }
                                }
                            }
                        }
                    }
                }

                if !usage.contains("markdown") {
                    continue;
                }

                if let Some(mb) = block.get("markdown_block") {
                    if let Some(chunks_arr) = mb.get("chunks").and_then(|c| c.as_array()) {
                        if chunks_arr.is_empty() {
                            continue;
                        }

                        let chunk_text: String = chunks_arr.iter().filter_map(|c| c.as_str()).collect();

                        if let Some("DONE") = mb.get("progress").and_then(|p| p.as_str()) {
                            full_answer = chunk_text;
                        } else {
                            let cumulative = format!("{}{}", full_answer, chunk_text);
                            if cumulative.len() > seen_len {
                                let delta = cumulative[seen_len..].to_string();
                                full_answer = cumulative.clone();
                                seen_len = cumulative.len();
                                chunks.push(ContentChunk::Delta { text: delta, answer: cumulative, backend_uuid: backend_uuid.clone() });
                            }
                        }
                    }
                }
            }
        }

        if let Some(text) = event.get("text").and_then(|t| t.as_str()) {
            let t = text.trim();
            if t.len() > seen_len {
                let delta = t[seen_len..].to_string();
                full_answer = t.to_string();
                seen_len = t.len();
                chunks.push(ContentChunk::Delta { text: delta, answer: t.to_string(), backend_uuid: backend_uuid.clone() });
            }
        }

        if event.get("final").and_then(|f| f.as_bool()) == Some(true) || event.get("status").and_then(|s| s.as_str()) == Some("COMPLETED") {
            break;
        }
    }

    chunks.push(ContentChunk::Done { answer: full_answer, backend_uuid });

    Ok(chunks)
}

/// Perplexity web executor — handles api.perplexity.ai web interface
pub struct PerplexityWebExecutor;

#[async_trait]
impl Executor for PerplexityWebExecutor {
    fn provider(&self) -> &str {
        "perplexity-web"
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        let messages = req.body.get("messages").and_then(|m| m.as_array()).ok_or_else(|| nr_core::Error::BadRequest("Missing or empty messages array".into()))?;

        if messages.is_empty() {
            return Err(nr_core::Error::BadRequest("Missing or empty messages array".into()));
        }

        let thinking = req.body.get("thinking").and_then(|t| t.as_bool()).unwrap_or(false) || (req.body.get("reasoning_effort").is_some() && req.body.get("reasoning_effort") != Some(&json!("none")));

        let (pplx_mode, model_pref) = if thinking {
            if let Some(&model_pref) = THINKING_MAP.get(req.model) {
                ("copilot", model_pref)
            } else {
                (MODEL_MAP.get(req.model).map(|(m, _)| *m).unwrap_or("copilot"), MODEL_MAP.get(req.model).map(|(_, p)| *p).unwrap_or(req.model))
            }
        } else if let Some(&(mode, pref)) = MODEL_MAP.get(req.model) {
            (mode, pref)
        } else {
            ("copilot", req.model)
        };

        let parsed = parse_openai_messages(messages);
        let follow_up_uuid = session_lookup(&parsed.history, &parsed.current_msg);

        let query = build_query(&parsed, follow_up_uuid.as_deref(), req.body.get("tools").and_then(|t| t.as_array()).map(|v| v.as_slice()));

        if query.trim().is_empty() {
            return Err(nr_core::Error::BadRequest("Empty query after processing".into()));
        }

        let pplx_body = build_pplx_request_body(&query, pplx_mode, model_pref, follow_up_uuid.as_deref());

        let mut headers = nr_core::http::HeaderList::new();
        headers.insert("Content-Type".to_string(), "application/json".to_string());
        headers.insert("Accept".to_string(), "text/event-stream".to_string());
        headers.insert("Origin".to_string(), "https://www.perplexity.ai".to_string());
        headers.insert("Referer".to_string(), "https://www.perplexity.ai/".to_string());
        headers.insert("User-Agent".to_string(), PPLX_USER_AGENT.to_string());
        headers.insert("X-App-ApiClient".to_string(), "default".to_string());
        headers.insert("X-App-ApiVersion".to_string(), PPLX_API_VERSION.to_string());

        if let Some(access_token) = req.credentials.access_token() {
            headers.insert("Authorization".to_string(), format!("Bearer {}", access_token));
        } else if let Some(api_key) = req.credentials.api_key() {
            headers.insert("Cookie".to_string(), format!("__Secure-next-auth.session-token={}", api_key));
        }

        let fetch_req = FetchRequest {
            method: http::Method::POST,
            url: PPLX_SSE_ENDPOINT.to_string(),
            headers: headers.clone(),
            body: Some(pplx_body.to_string().into()),
            cancel: Some(req.cancel.clone()),
            header_timeout: None,
            direct_fetch: None,
        };

        let response = proxy_fetch(fetch_req, Some(&req.proxy)).await?;

        if !response.ok() {
            let status = response.status;
            let err_msg = match status {
                401 | 403 => "Perplexity auth failed — session cookie may be expired. Re-paste your __Secure-next-auth.session-token.".to_string(),
                429 => "Perplexity rate limited. Wait a moment and retry.".to_string(),
                _ => format!("Perplexity returned HTTP {}", status),
            };

            return Ok(ExecResult {
                response: UpstreamResponse::json(status, &json!({ "error": { "message": err_msg, "type": "upstream_error" } })),
                url: PPLX_SSE_ENDPOINT.to_string(),
                headers,
                transformed_body: pplx_body,
                response_format: None,
            });
        }

        let body_bytes = response.bytes().await?;
        let events = read_pplx_sse_events(body_bytes).await?;
        let content_chunks = extract_content(&events).await?;

        let cid = format!("chatcmpl-pplx-{}", Uuid::new_v4().to_string()[..12].to_string());
        let created = (Utc::now().timestamp()) as i64;

        let response_stream = if req.stream {
            build_streaming_response(content_chunks.clone(), &cid, created, req.model, &parsed.history, &parsed.current_msg)
        } else {
            build_non_streaming_response(content_chunks, &cid, created, req.model, &parsed.history, &parsed.current_msg)?
        };

        Ok(ExecResult {
            response: response_stream,
            url: PPLX_SSE_ENDPOINT.to_string(),
            headers,
            transformed_body: pplx_body,
            response_format: None,
        })
    }
}

fn build_streaming_response(chunks: Vec<ContentChunk>, cid: &str, created: i64, model: &str, history: &[Value], current_msg: &str) -> UpstreamResponse {
    let mut responses = Vec::new();
    let mut full_answer = String::new();
    let mut backend_uuid: Option<String> = None;

    // Send initial role frame
    responses.push(sse_chunk(&json!({
        "id": cid,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{ "index": 0, "delta": { "role": "assistant" }, "finish_reason": Value::Null }]
    })));

    for chunk in chunks {
        match chunk {
            ContentChunk::Error { message } => {
                responses.push(sse_chunk(&json!({
                    "id": cid,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [{ "index": 0, "delta": { "content": format!("[Error: {}]", message) }, "finish_reason": Value::Null }]
                })));
                break;
            }
            ContentChunk::Thinking { text, .. } => {
                responses.push(sse_chunk(&json!({
                    "id": cid,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [{ "index": 0, "delta": { "reasoning_content": format!("{}\n", text) }, "finish_reason": Value::Null }]
                })));
            }
            ContentChunk::Delta { text, answer, backend_uuid: uuid } => {
                if let Some(uuid) = uuid {
                    backend_uuid = Some(uuid);
                }
                let cleaned = clean_response(&text, false);
                if !cleaned.is_empty() {
                    responses.push(sse_chunk(&json!({
                        "id": cid,
                        "object": "chat.completion.chunk",
                        "created": created,
                        "model": model,
                        "choices": [{ "index": 0, "delta": { "content": cleaned }, "finish_reason": Value::Null }]
                    })));
                }
                full_answer = answer;
            }
            ContentChunk::Done { answer, backend_uuid: uuid } => {
                if let Some(uuid) = uuid {
                    backend_uuid = Some(uuid);
                }
                full_answer = answer;
                break;
            }
        }
    }

    // Send final frame
    responses.push(sse_chunk(&json!({
        "id": cid,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }]
    })));
    responses.push("data: [DONE]\n\n".to_string());

    session_store(history, current_msg, &clean_response(&full_answer, true), backend_uuid.as_deref().unwrap_or(""));

    let body_stream = Box::pin(futures::stream::iter(responses.into_iter().map(|s| Ok::<Bytes, std::io::Error>(Bytes::from(s)))));

    UpstreamResponse::sse(body_stream)
}

fn build_non_streaming_response(chunks: Vec<ContentChunk>, cid: &str, created: i64, model: &str, history: &[Value], current_msg: &str) -> Result<UpstreamResponse> {
    let mut full_answer = String::new();
    let mut backend_uuid: Option<String> = None;
    let mut thinking_parts = Vec::new();

    for chunk in chunks {
        match chunk {
            ContentChunk::Error { message } => {
                return Ok(UpstreamResponse::json(
                    502,
                    &json!({
                        "error": { "message": message, "type": "upstream_error", "code": "PPLX_ERROR" }
                    }),
                ));
            }
            ContentChunk::Thinking { text, backend_uuid: uuid } => {
                thinking_parts.push(text);
                if let Some(uuid) = uuid {
                    backend_uuid = Some(uuid);
                }
            }
            ContentChunk::Delta { answer, backend_uuid: uuid, .. } => {
                if let Some(uuid) = uuid {
                    backend_uuid = Some(uuid);
                }
                full_answer = answer;
            }
            ContentChunk::Done { answer, backend_uuid: uuid } => {
                if let Some(uuid) = uuid {
                    backend_uuid = Some(uuid);
                }
                full_answer = answer;
                break;
            }
        }
    }

    full_answer = clean_response(&full_answer, true);
    session_store(history, current_msg, &full_answer, backend_uuid.as_deref().unwrap_or(""));

    let reasoning_content = if !thinking_parts.is_empty() { Some(thinking_parts.join("\n")) } else { None };

    let mut msg = json!({
        "role": "assistant",
        "content": full_answer,
    });
    if let Some(reasoning) = reasoning_content {
        msg["reasoning_content"] = json!(reasoning);
    }

    let prompt_tokens = (current_msg.len() as f64 / 4.0).ceil() as u32;
    let completion_tokens = (full_answer.len() as f64 / 4.0).ceil() as u32;

    Ok(UpstreamResponse::json(
        200,
        &json!({
            "id": cid,
            "object": "chat.completion",
            "created": created,
            "model": model,
            "choices": [{ "index": 0, "message": msg, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": prompt_tokens, "completion_tokens": completion_tokens, "total_tokens": prompt_tokens + completion_tokens }
        }),
    ))
}

/// Create the executor instance
pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(PerplexityWebExecutor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_name() {
        let exec = PerplexityWebExecutor;
        assert_eq!(exec.provider(), "perplexity-web");
    }

    #[test]
    fn test_parse_simple_messages() {
        let parsed = parse_openai_messages(&[
            json!({ "role": "system", "content": "Be helpful" }),
            json!({ "role": "user", "content": "Q1" }),
            json!({ "role": "assistant", "content": "A1" }),
            json!({ "role": "user", "content": "Q2" }),
        ]);

        assert_eq!(parsed.system_msg.trim(), "Be helpful");
        assert_eq!(parsed.history.len(), 2);
        assert_eq!(parsed.current_msg, "Q2");
    }

    #[test]
    fn test_parse_developer_as_system() {
        let parsed = parse_openai_messages(&[json!({ "role": "developer", "content": "Be concise" }), json!({ "role": "user", "content": "hi" })]);
        assert_eq!(parsed.system_msg.trim(), "Be concise");
    }

    #[test]
    fn test_parse_multipart_content() {
        let parsed = parse_openai_messages(&[json!({
            "role": "user",
            "content": [
                { "type": "text", "text": "part1" },
                { "type": "text", "text": "part2" }
            ]
        })]);
        assert_eq!(parsed.current_msg, "part1 part2");
    }

    #[test]
    fn test_clean_response() {
        let input = "[1] Some text [2] more [3]";
        let output = clean_response(input, true);
        assert!(output.contains("Some text"));
        assert!(!output.contains("[1]"));
    }

    #[test]
    fn test_clean_response_preserves_text_no_strip() {
        let input = "[1] Hello [2]";
        let output = clean_response(input, false);
        assert!(output.contains("Hello"));
        assert!(!output.contains("[1]"));
    }

    #[test]
    fn test_session_key_consistent() {
        let history = vec![json!({ "role": "user", "content": "hello" })];
        let key1 = session_key(&history, "test");
        let key2 = session_key(&history, "test");
        assert_eq!(key1, key2);
    }

    #[test]
    fn test_format_tools_empty() {
        assert_eq!(format_tools_hint(None), "");
        assert_eq!(format_tools_hint(Some(&[])), "");
    }

    #[test]
    fn test_format_tools_with_function() {
        let tools = vec![json!({
            "function": {
                "name": "Shell",
                "description": "Run shell commands"
            }
        })];
        let output = format_tools_hint(Some(&tools));
        assert!(output.contains("- Shell: Run shell commands"));
    }

    #[test]
    fn test_format_tools_flat_schema() {
        let tools = vec![json!({ "name": "Bar", "description": "does bar" })];
        let output = format_tools_hint(Some(&tools));
        assert!(output.contains("- Bar: does bar"));
    }

    #[test]
    fn test_build_pplx_body() {
        let body = build_pplx_request_body("hello", "concise", "pplx_pro", None);
        assert_eq!(body["query_str"], "hello");
        assert_eq!(body["params"]["query_str"], "hello");
        assert_eq!(body["params"]["mode"], "concise");
        assert_eq!(body["params"]["model_preference"], "pplx_pro");
    }

    #[test]
    fn test_build_pplx_body_with_follow_up() {
        let body = build_pplx_request_body("query", "copilot", "claude46sonnet", Some("uuid-abc"));
        assert_eq!(body["params"]["last_backend_uuid"], "uuid-abc");
    }

    #[test]
    fn test_build_query_first_turn() {
        let parsed = ParsedMessages {
            system_msg: "Be helpful\n".to_string(),
            history: vec![],
            current_msg: "Hello".to_string(),
        };
        let q = build_query(&parsed, None, None);
        let obj: Value = serde_json::from_str(&q).expect("should be JSON");
        assert_eq!(obj["query"], "Hello");
        assert!(obj.get("instructions").is_some());
    }

    #[test]
    fn test_build_query_follow_up() {
        let parsed = ParsedMessages {
            system_msg: String::new(),
            history: vec![json!({ "role": "user", "content": "earlier" })],
            current_msg: "Follow up".to_string(),
        };
        let q = build_query(&parsed, Some("uuid"), None);
        assert_eq!(q, "Follow up");
    }

    #[test]
    fn test_model_map() {
        assert!(MODEL_MAP.contains_key("pplx-auto"));
        assert_eq!(MODEL_MAP["pplx-auto"], ("concise", "pplx_pro"));
        assert!(THINKING_MAP.contains_key("pplx-opus"));
        assert_eq!(THINKING_MAP["pplx-opus"], "claude46opusthinking");
    }
}
