//! Port of open-sse/executors/chatgpt-web/chatHttp.js
//!
//! ChatGPT Web chat over plain HTTP.
//! - Sentinel headers solve the challenge layer
//! - `/f/conversation/prepare` → `/f/conversation` SSE with 120s deadline
//! - Poll `/backend-api/conversation/{id}` until `is_complete`
//! - Tool calling rides the `<tool>` contract (injected by translator)

use crate::delta_v1::ChatGptWebDeltaV1Decoder;
use crate::sentinel::fetch_sentinel_headers;
use nr_core::Result;
use regex::Regex;
use serde_json::{json, Value as Json};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use uuid::Uuid;

const SSE_DEADLINE_MS: u64 = 120000;
const POLL_INTERVAL_MS: u64 = 1500;
const POLL_TIMEOUT_MS: u64 = 180000;
const MAX_SSE_BYTES: usize = 16 * 1024 * 1024;

/// ChatGPT ignores the shared tool contract unless told explicitly in the last turn.
const TOOL_ORDER: &str =
    "Respond with ONLY <tool> blocks, one per line. No prose, no code fences, no explanation before or after the blocks.";
const TOOL_LEAD: &str =
    "Use the client tools provided below. Do not answer with prose when a tool is needed.";

/// Extract text content from a message part (handles strings and text objects).
fn content_text(part: &Json) -> String {
    if let Some(s) = part.as_str() {
        s.to_string()
    } else if let Some(obj) = part.as_object() {
        if obj.get("type").and_then(|t| t.as_str()) == Some("text")
            || obj.get("type").and_then(|t| t.as_str()) == Some("input_text")
        {
            obj.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string()
        } else {
            String::new()
        }
    } else {
        String::new()
    }
}

/// Flatten OpenAI messages into one prompt turn (the web app has no role array).
fn flatten_messages(messages: &[Json]) -> Result<String> {
    let mut system_lines = Vec::new();
    let mut lines = Vec::new();

    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let mut parts = Vec::new();

        if let Some(content) = msg.get("content") {
            if let Some(s) = content.as_str() {
                parts.push(s.to_string());
            } else if let Some(arr) = content.as_array() {
                for item in arr {
                    if let Some(s) = item.as_str() {
                        parts.push(s.to_string());
                    } else if let Some(obj) = item.as_object() {
                        if obj.get("type").and_then(|t| t.as_str()) == Some("text") {
                            if let Some(text) = obj.get("text").and_then(|t| t.as_str()) {
                                parts.push(text.to_string());
                            }
                        }
                    }
                }
            }
        }

        if let Some(tool_calls) = msg.get("tool_calls").and_then(|tc| tc.as_array()) {
            for call in tool_calls {
                if let Some(fn_obj) = call.get("function").and_then(|f| f.as_object()) {
                    if let Some(name) = fn_obj.get("name").and_then(|n| n.as_str()) {
                        let mut args = fn_obj
                            .get("arguments")
                            .and_then(|a| a.as_str())
                            .unwrap_or("{}")
                            .to_string();
                        if args.is_empty() || args == "{}" {
                            args = "{}".to_string();
                        }
                        parts.push(format!(r#"<tool>{{"name": "{}", "arguments": {}}}</tool>"#, name, args));
                    }
                }
            }
        }

        let text = parts.join("\n").trim().to_string();
        if text.is_empty() {
            continue;
        }

        if role == "system" || role == "developer" {
            system_lines.push(text);
        } else if role == "tool" {
            lines.push(format!(
                "Tool result{}: {}",
                msg.get("tool_call_id")
                    .and_then(|id| id.as_str())
                    .map(|id| format!(" ({})", id))
                    .unwrap_or_default(),
                text
            ));
        } else if role == "assistant" {
            lines.push(format!("Assistant: {}", text));
        } else {
            lines.push(text);
        }
    }

    let mut all_lines = system_lines;
    all_lines.extend(lines);
    Ok(all_lines.join("\n\n"))
}

/// The strict tool order is only safe while the model still has tools to call.
/// Once the conversation carries tool results the model is summarizing,
/// forcing the order makes it call the same tool again.
fn with_tool_order(prompt: &str, has_tools: bool, messages: &[Json]) -> String {
    if !has_tools {
        return prompt.to_string();
    }
    let has_tool_results = messages.iter().any(|msg| {
        msg.get("role")
            .and_then(|r| r.as_str())
            .map(|r| r.to_lowercase() == "tool")
            .unwrap_or(false)
    });
    if has_tool_results {
        prompt.to_string()
    } else {
        format!("{}\n\n{}\n\n{}", TOOL_LEAD, prompt, TOOL_ORDER)
    }
}

/// Collect finished assistant text from a conversation document.
/// Returns None while the turn has not finished yet.
fn finished_assistant_text(document: &Json) -> Option<(String, String)> {
    let mapping = document.get("mapping")?.as_object()?;
    let mut text = String::new();
    let mut complete = false;
    let mut error_text = String::new();

    for node in mapping.values() {
        let message = node.get("message")?;
        let role = message
            .get("author")
            .and_then(|a| a.get("role"))
            .and_then(|r| r.as_str())
            .map(|r| r.to_lowercase())
            .unwrap_or_default();

        if role == "assistant" {
            if message.get("metadata")
                .and_then(|m| m.get("is_complete"))
                .and_then(|ic| ic.as_bool())
                == Some(true)
            {
                complete = true;
            }

            if message
                .get("content")
                .and_then(|c| c.get("content_type"))
                .and_then(|ct| ct.as_str())
                == Some("text")
            {
                if let Some(parts) = message.get("content").and_then(|c| c.get("parts")) {
                    if let Some(arr) = parts.as_array() {
                        for part in arr {
                            if let Some(s) = part.as_str() {
                                text.push_str(s);
                            }
                        }
                    }
                }
            }

            if message
                .get("metadata")
                .and_then(|m| m.get("is_error"))
                .and_then(|ie| ie.as_bool())
                == Some(true)
            {
                if let Some(parts) = message.get("content").and_then(|c| c.get("parts")) {
                    if let Some(arr) = parts.as_array() {
                        for part in arr {
                            if let Some(s) = part.as_str() {
                                error_text = s.to_string();
                            }
                        }
                    }
                }
            }
        }
    }

    if !complete {
        return None;
    }
    Some((text, error_text))
}

/// HTTP context bundling auth headers and state.
struct HttpContext {
    base_url: String,
    access_token: String,
    cookie: String,
    user_agent: String,
    headers: HashMap<String, String>,
}

impl HttpContext {
    fn new(
        base_url: String,
        access_token: String,
        cookie: String,
        user_agent: String,
        sentinel: &HashMap<String, String>,
    ) -> Self {
        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), format!("Bearer {}", access_token));
        headers.insert("User-Agent".to_string(), user_agent.clone());
        headers.insert("Content-Type".to_string(), "application/json".to_string());
        if !cookie.is_empty() {
            headers.insert("Cookie".to_string(), cookie.clone());
        }
        // Mix in sentinel headers
        headers.extend(sentinel.iter().map(|(k, v)| (k.clone(), v.clone())));

        Self {
            base_url,
            access_token,
            cookie,
            user_agent,
            headers,
        }
    }
}

pub struct ChatHttpTurnResult {
    pub text: String,
    pub has_tools: bool,
    pub requested_tools: Vec<String>,
}

/// Run one chat turn over HTTP.
pub async fn run_chat_gpt_web_http_turn(
    base_url: String,
    access_token: String,
    cookie: String,
    user_agent: String,
    body: &Json,
    has_tools: bool,
    requested_tools: Vec<String>,
    _signal: Option<&tokio::sync::Notify>,
) -> Result<ChatHttpTurnResult, String> {
    if access_token.is_empty() {
        return Err("ChatGPT session has no access token".to_string());
    }

    let messages = body
        .get("messages")
        .and_then(|m| m.as_array())
        .ok_or("ChatGPT Web requires messages")?;
    if messages.is_empty() {
        return Err("ChatGPT Web requires messages".to_string());
    }

    let prompt = flatten_messages(messages).map_err(|e| e.to_string())?;
    let prompt = with_tool_order(&prompt, has_tools, messages);

    if prompt.trim().is_empty() {
        return Err("ChatGPT Web requires non-empty text".to_string());
    }

    let sentinel = fetch_sentinel_headers(&base_url, &access_token, &user_agent)
        .await
        .map_err(|e| format!("Failed to fetch sentinel headers: {}", e))?;

    let ctx = HttpContext::new(base_url.clone(), access_token.clone(), cookie.clone(), user_agent.clone(), &sentinel);

    // POST /backend-api/f/conversation/prepare
    let prepare_url = format!("{}/backend-api/f/conversation/prepare", ctx.base_url);
    let prepare_body = json!({
        "action": "next",
        "parent_message_id": Uuid::new_v4().to_string(),
        "model": "auto",
        "client_prepare_state": "success",
        "timezone_offset_min": 0,
        "timezone": "UTC",
        "conversation_mode": { "kind": "primary_assistant" },
        "partial_query": {
            "id": Uuid::new_v4().to_string(),
            "author": { "role": "user" },
            "content": { "content_type": "text", "parts": [&prompt] }
        },
        "supports_buffering": true,
        "supported_encodings": ["v1"],
        "client_contextual_info": { "app_name": "chatgpt.com" }
    });

    let client = reqwest::Client::new();
    let mut prepare_headers = ctx.headers.clone();
    prepare_headers.insert("Accept".to_string(), "application/json".to_string());

    let prepare_req = client
        .post(&prepare_url)
        .headers(prepare_headers.iter().map(|(k, v)| (k.parse().unwrap(), v.parse().unwrap())).collect())
        .json(&prepare_body)
        .timeout(Duration::from_secs(60));

    let prepare_resp = prepare_req
        .send()
        .await
        .map_err(|e| format!("ChatGPT prepare failed: {}", e))?;

    if !prepare_resp.status().is_success() {
        let status = prepare_resp.status().as_u16();
        return Err(format!("ChatGPT prepare failed (HTTP {})", status));
    }

    let prepare_json = prepare_resp
        .json::<Json>()
        .await
        .unwrap_or(Json::Null);
    let conduit_token = prepare_json
        .get("conduit_token")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();

    // POST /backend-api/f/conversation (SSE)
    let conversation_url = format!("{}/backend-api/f/conversation", ctx.base_url);
    let conversation_body = json!({
        "action": "next",
        "messages": [{
            "id": Uuid::new_v4().to_string(),
            "author": { "role": "user" },
            "create_time": (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as f64),
            "content": { "content_type": "text", "parts": [&prompt] },
            "metadata": { "serialization_metadata": { "custom_symbol_offsets": [] } }
        }],
        "parent_message_id": Uuid::new_v4().to_string(),
        "model": "auto",
        "client_prepare_state": "sent",
        "timezone_offset_min": 0,
        "timezone": "UTC",
        "conversation_mode": { "kind": "primary_assistant" },
        "enable_message_followups": true,
        "supports_buffering": true,
        "supported_encodings": ["v1"],
        "client_contextual_info": { "app_name": "chatgpt.com" },
        "force_parallel_switch": "auto"
    });

    let mut conversation_headers = ctx.headers.clone();
    conversation_headers.insert("Accept".to_string(), "text/event-stream".to_string());
    conversation_headers.insert("X-Conduit-Token".to_string(), conduit_token);
    conversation_headers.insert("X-Oai-Turn-Trace-Id".to_string(), Uuid::new_v4().to_string());

    let conversation_req = client
        .post(&conversation_url)
        .headers(conversation_headers.iter().map(|(k, v)| (k.parse().unwrap(), v.parse().unwrap())).collect())
        .json(&conversation_body)
        .timeout(Duration::from_millis(SSE_DEADLINE_MS + 15000));

    let conversation_resp = conversation_req
        .send()
        .await
        .map_err(|e| format!("ChatGPT conversation failed: {}", e))?;

    if !conversation_resp.status().is_success() {
        return Err(format!("ChatGPT conversation failed (HTTP {})", conversation_resp.status().as_u16()));
    }

    // Read SSE stream with deadline
    let mut sse_text = String::new();
    let _decoder = ChatGptWebDeltaV1Decoder::new();
    let deadline = Instant::now() + Duration::from_millis(SSE_DEADLINE_MS);
    let mut stream = conversation_resp.bytes_stream();
    use futures::StreamExt;

    while let Some(chunk) = stream.next().await {
        if Instant::now() > deadline {
            break;
        }
        let chunk = chunk.map_err(|e| format!("Stream error: {}", e))?;
        sse_text.push_str(&String::from_utf8_lossy(&chunk));
        if sse_text.len() > MAX_SSE_BYTES {
            break;
        }
    }

    // Extract conversation ID from SSE text
    let conversation_id_re = Regex::new(r#""conversation_id"\s*:\s*"([0-9a-f-]{36})""#)
        .unwrap();
    let conversation_id = conversation_id_re
        .captures(&sse_text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
        .unwrap_or("");

    if conversation_id.is_empty() {
        return Err("ChatGPT conversation produced no conversation id".to_string());
    }

    // Poll /backend-api/conversation/{id} until is_complete
    let started = Instant::now();
    loop {
        if started.elapsed() > Duration::from_millis(POLL_TIMEOUT_MS) {
            return Err("ChatGPT chat turn timed out".to_string());
        }

        sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;

        let poll_url = format!("{}/backend-api/conversation/{}", ctx.base_url, conversation_id);
        let mut poll_headers = ctx.headers.clone();
        poll_headers.insert("Accept".to_string(), "application/json".to_string());

        let poll_req = client
            .get(&poll_url)
            .headers(poll_headers.iter().map(|(k, v)| (k.parse().unwrap(), v.parse().unwrap())).collect())
            .timeout(Duration::from_secs(30));

        if let Ok(poll_resp) = poll_req.send().await {
            if poll_resp.status().is_success() {
                if let Ok(document) = poll_resp.json::<Json>().await {
                    if let Some((text, error_text)) = finished_assistant_text(&document) {
                        if !text.trim().is_empty() {
                            return Ok(ChatHttpTurnResult {
                                text,
                                has_tools,
                                requested_tools,
                            });
                        }
                        if !error_text.is_empty() {
                            return Err(format!("ChatGPT refused the turn: {}", &error_text[..200.min(error_text.len())]).into());
                        }
                    }
                }
            }
        }
    }
}
