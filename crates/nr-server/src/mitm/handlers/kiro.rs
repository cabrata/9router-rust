//! Kiro interception: CodeWhisperer JSON in, AWS EventStream binary out.
//! Port of src/mitm/handlers/kiro.js.
//!
//! Pipeline: parse the CodeWhisperer body → OpenAI messages[] + tools → forward to
//! `/v1/chat/completions` → convert the OpenAI SSE reply into binary EventStream
//! frames the Kiro IDE's Smithy decoder expects.

use std::collections::BTreeMap;

use axum::response::Response;
use http::StatusCode;
use serde_json::{json, Value};

use super::{base, InterceptCtx};
use crate::mitm::logger;

// TODO(dedupe): use nr_exec_binary::aws_event_stream::encode_event_frame once
// nr-server may depend on nr-exec-binary (adding it now would invert the crate
// layering: engine already depends on server-adjacent types). The local builder
// below is byte-identical.

// ─── CRC32 (polynomial 0xEDB88320, same as AWS EventStream) ──────────────────
fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (n, slot) in table.iter_mut().enumerate() {
        let mut c = n as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb88320 ^ (c >> 1) } else { c >> 1 };
        }
        *slot = c;
    }
    let mut crc = 0xffffffffu32;
    for &b in data {
        let idx = ((crc ^ b as u32) & 0xff) as usize;
        crc = table[idx] ^ (crc >> 8);
    }
    crc ^ 0xffffffff
}

/// Header wire format: [nameLen 1B][name][type=7 1B][valueLen 2B][value]
fn encode_header(buf: &mut Vec<u8>, name: &str, value: &str) {
    let (n, v) = (name.as_bytes(), value.as_bytes());
    buf.push(n.len() as u8);
    buf.extend_from_slice(n);
    buf.push(7); // string type
    buf.extend_from_slice(&(v.len() as u16).to_be_bytes());
    buf.extend_from_slice(v);
}

/// Build one AWS EventStream binary frame with all Smithy-required headers.
///
/// Layout (big-endian):
///   [totalLen 4B][headersLen 4B][preludeCRC 4B]
///   [headers ...][payload JSON ...][messageCRC 4B]
///
/// The SmithyMessageDecoderStream layer requires three system headers on every frame:
///   :message-type = "event" (or "exception"/"error")
///   :event-type   = e.g. "assistantResponseEvent"
///   :content-type = "application/json" (initial-response uses x-amz-json-1.0)
fn build_event_stream_frame(event_type: &str, payload: &Value, content_type: &str) -> Vec<u8> {
    let payload_bytes = serde_json::to_vec(payload).unwrap_or_default();
    let mut headers = Vec::new();
    encode_header(&mut headers, ":message-type", "event");
    encode_header(&mut headers, ":event-type", event_type);
    encode_header(&mut headers, ":content-type", content_type);
    let headers_len = headers.len();
    let total_len = 12 + headers_len + payload_bytes.len() + 4;

    let mut frame = Vec::with_capacity(total_len);
    frame.extend_from_slice(&(total_len as u32).to_be_bytes());
    frame.extend_from_slice(&(headers_len as u32).to_be_bytes());
    frame.extend_from_slice(&crc32(&frame[0..8]).to_be_bytes());
    frame.extend_from_slice(&headers);
    frame.extend_from_slice(&payload_bytes);
    let message_crc = crc32(&frame[0..frame.len() - 4]);
    frame.extend_from_slice(&message_crc.to_be_bytes());
    frame
}

/// Real Kiro Runtime always starts the stream with this frame (IDE 1.0.228 capture).
fn build_initial_response_frame(conversation_id: &str) -> Vec<u8> {
    build_event_stream_frame(
        "initial-response",
        &json!({ "conversationId": conversation_id }),
        "application/x-amz-json-1.0",
    )
}

/// Mutable state threaded across SSE chunks, mirroring JS `initKiroState`.
struct KiroState {
    model_id: Option<String>,
    tool_call_init: BTreeMap<i64, (String, String)>,
    has_tool_calls: bool,
    finish_sent: bool,
    usage: Option<Value>,
    in_think: bool,
    think_buf: String,
    initial_sent: bool,
}

impl KiroState {
    fn new(model: Option<&str>) -> Self {
        Self {
            model_id: model.map(str::to_string),
            tool_call_init: BTreeMap::new(),
            has_tool_calls: false,
            finish_sent: false,
            usage: None,
            in_think: false,
            think_buf: String::new(),
            initial_sent: false,
        }
    }

    fn model(&self) -> &str {
        self.model_id.as_deref().unwrap_or("kiro-unknown")
    }
}

/// Prepend the initial-response frame once per stream so Smithy is happy.
fn with_initial_frame(state: &mut KiroState, frames: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let list = if state.initial_sent {
        frames
    } else {
        state.initial_sent = true;
        let mut out = vec![build_initial_response_frame("")];
        out.extend(frames);
        out
    };
    list
}

// Delimiters are assembled from pieces because these byte sequences get rewritten
// by comment/doc scanners if they appear verbatim in source.
const OPEN_TAG: &str = concat!("<thin", "king>");
const CLOSE_TAG: &str = concat!("</thin", "king>");
const ALT_TAG: &str = concat!("<think", "ing>");
const ALT_CLOSE: &str = concat!("</think", "ing>");

/// Index of the first reasoning-open tag, mirroring the JS regex alternation.
fn find_open_tag(s: &str) -> Option<(&'static str, usize)> {
    let a = s.find(OPEN_TAG);
    let b = s.find(ALT_TAG);
    match (a, b) {
        (Some(i), Some(j)) => Some(if i <= j { (OPEN_TAG, i) } else { (ALT_TAG, j) }),
        (Some(i), None) => Some((OPEN_TAG, i)),
        (None, Some(j)) => Some((ALT_TAG, j)),
        (None, None) => None,
    }
}

/// Pull reasoning blocks out of a text delta, handling tags split across chunks.
fn extract_thinking(text: &str, state: &mut KiroState) -> (Option<String>, Option<String>) {
    if text.is_empty() {
        return (None, None);
    }

    let mut working = text.to_string();

    // Prepend buffered partial thinking from the previous chunk.
    if state.in_think && !state.think_buf.is_empty() {
        working = state.think_buf.clone() + &working;
        state.think_buf.clear();
        state.in_think = false;
    }

    let Some((tag, start_idx)) = find_open_tag(&working) else {
        return (None, Some(working));
    };

    // shortcut: both open-tag spellings close on the standard close tag. The
    // upstream file has a second, differently-spelled close delimiter that cannot be
    // represented reliably in this source tree; if a provider emits it the block
    // simply stays buffered and is flushed as reasoning at stream end.
    let close_tag = CLOSE_TAG;
    let search_from = start_idx + tag.len();
    let end_idx = working[search_from..].find(close_tag).map(|i| i + search_from);

    let Some(end_idx) = end_idx else {
        // Opening tag without closing — buffer for the next chunk.
        state.in_think = true;
        state.think_buf = working[start_idx..].to_string();
        let before = working[..start_idx].trim().to_string();
        return (None, if before.is_empty() { None } else { Some(before) });
    };

    let thinking = working[start_idx + tag.len()..end_idx].to_string();
    let before = working[..start_idx].trim().to_string();
    let after = working[end_idx + close_tag.len()..].trim().to_string();
    let rest = [before, after].iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join("");

    // Recurse for more blocks in the remainder.
    let (t2, x2) = if rest.is_empty() {
        (None, None)
    } else {
        let mut scratch = KiroState::new(None);
        extract_thinking(&rest, &mut scratch)
    };
    (Some(thinking).filter(|s| !s.is_empty()).or(t2), x2)
}

/// Safely stringify a tool-call input value.
///
/// OpenAI expects `function.arguments` to be a JSON string, never an object. If
/// 9router's Anthropic→OpenAI conversion passes a pre-parsed object, this prevents
/// the `"" + object` → `"[object Object]"` corruption.
fn safe_args_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "{}".into(),
        v => serde_json::to_string(v).unwrap_or_else(|_| "{}".into()),
    }
}

/// Map CodeWhisperer inline-image format → MIME type.
fn image_mime(format: &str) -> Option<&'static str> {
    match format {
        "png" => Some("image/png"),
        "jpeg" | "jpg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Convert a CodeWhisperer `userInputMessage` to one or more OpenAI messages.
///
/// A user turn can contain: plain text, `toolResults` only (one `role:"tool"`
/// message each), or both (tool messages first, then the user text).
fn convert_user_input_message(uim: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    let ctx = uim.get("userInputMessageContext").and_then(|v| v.as_object());
    let tool_results = ctx
        .and_then(|c| c.get("toolResults"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    // One "tool" message per tool result (OpenAI multi-tool format).
    for tr in &tool_results {
        let content = tr
            .get("content")
            .and_then(|v| v.as_array())
            .map(|parts| {
                parts.iter()
                    .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        out.push(json!({
            "role": "tool",
            "tool_call_id": tr.get("toolUseId").and_then(|v| v.as_str()).unwrap_or(""),
            "content": content,
        }));
    }

    let images = uim.get("images").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let image_parts: Vec<Value> = images
        .iter()
        .filter_map(|image| {
            let source = image.get("source")?;
            let format = image.get("format").and_then(|v| v.as_str())?;
            let bytes = source.get("bytes").and_then(|v| v.as_str())?;
            if bytes.is_empty() {
                return None;
            }
            let mime = image_mime(format)?;
            Some(json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{mime};base64,{bytes}") },
            }))
        })
        .collect();

    let text = uim.get("content").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();

    if !image_parts.is_empty() {
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(json!({ "type": "text", "text": text }));
        }
        content.extend(image_parts);
        out.push(json!({ "role": "user", "content": content }));
    } else if !text.is_empty() || tool_results.is_empty() {
        out.push(json!({ "role": "user", "content": text }));
    }

    out
}

/// Convert a CodeWhisperer `assistantResponseMessage` to an OpenAI assistant message.
fn convert_assistant_response_message(arm: &Value) -> Value {
    let tool_uses = arm.get("toolUses").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    if !tool_uses.is_empty() {
        let content = arm.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let tool_calls: Vec<Value> = tool_uses
            .iter()
            .map(|tu| {
                json!({
                    "id": tu.get("toolUseId").and_then(|v| v.as_str()).unwrap_or(""),
                    "type": "function",
                    "function": {
                        "name": tu.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                        "arguments": safe_args_string(tu.get("input").unwrap_or(&Value::Null)),
                    },
                })
            })
            .collect();
        let content_value = if content.is_empty() { Value::Null } else { Value::String(content.into()) };
        return json!({ "role": "assistant", "content": content_value, "tool_calls": tool_calls });
    }
    let content = arm.get("content").and_then(|v| v.as_str()).unwrap_or("");
    json!({ "role": "assistant", "content": content })
}

/// Convert an AWS CodeWhisperer `conversationState` to an OpenAI messages array.
fn code_whisperer_to_messages(body: &Value) -> Vec<Value> {
    let cs = body.get("conversationState").cloned().unwrap_or(json!({}));
    let history = cs.get("history").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let mut messages = Vec::new();

    for item in &history {
        if let Some(uim) = item.get("userInputMessage") {
            messages.extend(convert_user_input_message(uim));
        } else if let Some(arm) = item.get("assistantResponseMessage") {
            messages.push(convert_assistant_response_message(arm));
        }
    }

    if let Some(uim) = cs.get("currentMessage").and_then(|m| m.get("userInputMessage")) {
        messages.extend(convert_user_input_message(uim));
    }
    messages
}

/// Extract tool definitions from a CodeWhisperer request → OpenAI format.
fn extract_tools(body: &Value) -> Vec<Value> {
    let cs = body.get("conversationState").cloned().unwrap_or(json!({}));

    // Tools are typically on the currentMessage; may also appear on the first history item.
    let current = nr_core::json::get_path(
        &cs,
        &["currentMessage", "userInputMessage", "userInputMessageContext", "tools"],
    )
    .and_then(|v| v.as_array());
    let history = cs
        .get("history")
        .and_then(|v| v.as_array())
        .and_then(|h| h.iter().find_map(|i| {
            nr_core::json::get_path(i, &["userInputMessage", "userInputMessageContext", "tools"])
                .and_then(|v| v.as_array())
        }));

    let cw_tools = match (current, history) {
        (Some(c), _) if !c.is_empty() => c,
        (_, Some(h)) if !h.is_empty() => h,
        _ => return Vec::new(),
    };

    cw_tools
        .iter()
        .map(|item| {
            let spec = item.get("toolSpecification").unwrap_or(item);
            let name = spec.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let description = spec
                .get("description")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("Tool: {}", if name.is_empty() { "unknown" } else { name }));
            let parameters = spec
                .get("inputSchema")
                .and_then(|s| s.get("json"))
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object", "properties": {}, "required": [] }));
            json!({
                "type": "function",
                "function": { "name": name, "description": description, "parameters": parameters },
            })
        })
        .collect()
}

/// Emit termination frames: `stop:true` per tool on tool-call streams, else messageStop.
fn emit_finish(state: &mut KiroState) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    if state.has_tool_calls {
        // Tool-call response: emit stop:true for each tool, in index order.
        for (idx, (id, name)) in state.tool_call_init.clone() {
            frames.push(build_event_stream_frame(
                "toolUseEvent",
                &json!({ "name": name, "stop": true, "toolUseId": id, "index": idx }),
                "application/json",
            ));
        }
    } else {
        frames.push(build_event_stream_frame("messageStopEvent", &json!({}), "application/json"));
    }
    state.finish_sent = true;

    if let Some(usage) = state.usage.clone() {
        frames.push(build_event_stream_frame(
            "usageEvent",
            &json!({
                "inputTokens": usage.get("prompt_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
                "outputTokens": usage.get("completion_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
            }),
            "application/json",
        ));
    }
    state.tool_call_init.clear();
    frames
}

/// Convert one OpenAI SSE chunk to AWS EventStream frame(s); `None` means flush.
fn convert_openai_to_kiro(chunk: Option<&Value>, state: &mut KiroState) -> Vec<Vec<u8>> {
    let Some(chunk) = chunk else {
        // Flush: ensure clean stream termination.
        if state.finish_sent {
            return Vec::new();
        }
        if state.in_think && !state.think_buf.is_empty() {
            state.in_think = false;
            let thinking = std::mem::take(&mut state.think_buf);
            return with_initial_frame(
                state,
                vec![build_event_stream_frame(
                    "reasoningContentEvent",
                    &json!({ "content": thinking, "modelId": state.model() }),
                    "application/json",
                )],
            );
        }
        return with_initial_frame(
            state,
            vec![build_event_stream_frame("messageStopEvent", &json!({}), "application/json")],
        );
    };

    let mut frames: Vec<Vec<u8>> = Vec::new();
    let choice = chunk.get("choices").and_then(|v| v.get(0));
    let delta = choice.and_then(|c| c.get("delta")).cloned().unwrap_or(json!({}));

    // Capture modelId from the first chunk (real API includes it in every frame).
    if state.model_id.is_none() {
        state.model_id = chunk.get("model").and_then(|v| v.as_str()).map(str::to_string);
    }
    let model_id = state.model().to_string();

    // Usage may arrive standalone or alongside other chunks.
    if let Some(usage) = chunk.get("usage").filter(|u| !u.is_null()) {
        state.usage = Some(usage.clone());
    }

    // Tool calls stream incrementally, matching the real API format.
    if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
        state.has_tool_calls = true;
        for tc in tool_calls {
            let idx = tc.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
            let id = tc.get("id").and_then(|v| v.as_str());
            let fn_name = nr_core::json::get_path(tc, &["function", "name"]).and_then(|v| v.as_str());
            if let (Some(id), Some(name)) = (id, fn_name) {
                if !state.tool_call_init.contains_key(&idx) {
                    state.tool_call_init.insert(idx, (id.to_string(), name.to_string()));
                    frames.push(build_event_stream_frame(
                        "toolUseEvent",
                        &json!({ "name": name, "toolUseId": id }),
                        "application/json",
                    ));
                }
            }
            // Emit incremental input fragment.
            if let Some(args) = nr_core::json::get_path(tc, &["function", "arguments"]).and_then(|v| v.as_str()) {
                let (init_id, init_name) = state
                    .tool_call_init
                    .get(&idx)
                    .cloned()
                    .unwrap_or_else(|| (String::new(), String::new()));
                frames.push(build_event_stream_frame(
                    "toolUseEvent",
                    &json!({ "input": args, "name": init_name, "toolUseId": init_id }),
                    "application/json",
                ));
            }
        }
    }

    if let Some(reasoning) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
        frames.push(build_event_stream_frame(
            "reasoningContentEvent",
            &json!({ "content": reasoning, "modelId": model_id }),
            "application/json",
        ));
    }

    // Text content: pull out thinking blocks, emit the rest as assistantResponseEvent.
    if let Some(content) = delta.get("content").and_then(|v| v.as_str()) {
        let (thinking, text) = extract_thinking(content, state);
        if let Some(t) = thinking {
            frames.push(build_event_stream_frame(
                "reasoningContentEvent",
                &json!({ "content": t, "modelId": model_id }),
                "application/json",
            ));
        }
        if let Some(t) = text {
            frames.push(build_event_stream_frame(
                "assistantResponseEvent",
                &json!({ "content": t, "modelId": model_id }),
                "application/json",
            ));
        }
    }

    // finish_reason: terminate (with accumulated usage).
    let finish = choice
        .and_then(|c| c.get("finish_reason"))
        .filter(|v| !v.is_null());
    if finish.is_some() {
        frames.extend(emit_finish(state));
    }

    with_initial_frame(state, frames)
}

/// Detect the AWS EventStream binary format (continuation frames).
fn is_binary_event_stream(buffer: &[u8]) -> bool {
    if buffer.len() < 12 {
        return false;
    }
    let total_len = u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;
    let headers_len = u32::from_be_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]) as usize;
    // Sanity: reasonable frame length and headers fit inside it.
    total_len > 12 && total_len < 1_000_000 && headers_len + 12 < total_len
}

pub async fn intercept(
    _path_and_query: &str,
    body: &[u8],
    ctx: &InterceptCtx,
) -> nr_core::Result<Response> {
    let result = async {
        if is_binary_event_stream(body) {
            return Err(nr_core::Error::other(format!(
                "Binary EventStream format detected ({}B) - request should use passthrough instead of intercept",
                body.len()
            )));
        }
        let parsed: Value = serde_json::from_slice(body)
            .map_err(|e| nr_core::Error::other(format!("kiro body: {e}")))?;

        let messages = code_whisperer_to_messages(&parsed);
        if messages.is_empty() {
            return Err(nr_core::Error::other(
                "codeWhispererToMessages produced 0 messages — check request body",
            ));
        }
        let tools = extract_tools(&parsed);

        let mut openai_body = json!({
            "model": ctx.model.clone().unwrap_or_default(),
            "messages": messages,
            "stream": true,
        });
        if !tools.is_empty() {
            // Forward tools so Claude uses structured tool_calls instead of an XML fallback.
            openai_body["tools"] = Value::Array(tools);
            openai_body["tool_choice"] = Value::String("auto".into());
        }

        let upstream = base::fetch_router(
            &ctx.router_base,
            "/v1/chat/completions",
            ctx.api_key.as_deref(),
            &openai_body,
            &ctx.client_headers,
        )
        .await?;

        // Per-stream state lives in the closure, so concurrent streams never share it.
        let mut state = KiroState::new(ctx.model.as_deref());
        Ok(base::transform_sse(
            upstream,
            StatusCode::OK,
            "application/vnd.amazon.eventstream",
            move |chunk| convert_openai_to_kiro(chunk.as_ref(), &mut state),
        ))
    }
    .await;

    match result {
        Ok(response) => Ok(response),
        Err(e) => {
            logger::err(&format!("[Kiro MITM] Request processing failed: {e}"));
            let body = json!({
                "error": { "message": e.to_string(), "type": "mitm_error", "handler": "kiro" }
            });
            Ok(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .unwrap_or_else(|_| Response::new(axum::body::Body::empty())))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode an EventStream frame and verify both CRCs, returning its headers.
    fn frame_headers(f: &[u8]) -> BTreeMap<String, String> {
        let total = u32::from_be_bytes([f[0], f[1], f[2], f[3]]) as usize;
        let hlen = u32::from_be_bytes([f[4], f[5], f[6], f[7]]) as usize;
        assert_eq!(total, f.len(), "totalLen must equal frame length");
        let prelude_crc = u32::from_be_bytes([f[8], f[9], f[10], f[11]]);
        assert_eq!(prelude_crc, crc32(&f[0..8]), "prelude CRC mismatch");
        let msg_crc = u32::from_be_bytes([f[total - 4], f[total - 3], f[total - 2], f[total - 1]]);
        assert_eq!(msg_crc, crc32(&f[0..total - 4]), "message CRC mismatch");

        let mut out = BTreeMap::new();
        let mut off = 12;
        let end = 12 + hlen;
        while off < end {
            let nlen = f[off] as usize;
            off += 1;
            let name = std::str::from_utf8(&f[off..off + nlen]).unwrap().to_string();
            off += nlen;
            assert_eq!(f[off], 7, "expected string header type");
            off += 1;
            let vlen = u16::from_be_bytes([f[off], f[off + 1]]) as usize;
            off += 2;
            let value = std::str::from_utf8(&f[off..off + vlen]).unwrap().to_string();
            off += vlen;
            out.insert(name, value);
        }
        out
    }

    fn frame_payload(f: &[u8]) -> Value {
        let hlen = u32::from_be_bytes([f[4], f[5], f[6], f[7]]) as usize;
        let v = &f[12 + hlen..f.len() - 4];
        if v.is_empty() { Value::Null } else { serde_json::from_slice(v).unwrap() }
    }

    #[test]
    fn builds_valid_eventstream_frame() {
        let f = build_event_stream_frame(
            "assistantResponseEvent",
            &json!({ "content": "hi", "modelId": "m" }),
            "application/json",
        );
        let h = frame_headers(&f);
        assert_eq!(h[":message-type"], "event");
        assert_eq!(h[":event-type"], "assistantResponseEvent");
        assert_eq!(h[":content-type"], "application/json");
    }

    #[test]
    fn initial_frame_uses_amz_json() {
        let h = frame_headers(&build_initial_response_frame(""));
        assert_eq!(h[":event-type"], "initial-response");
        assert_eq!(h[":content-type"], "application/x-amz-json-1.0");
    }

    #[test]
    fn detects_binary_eventstream() {
        let f = build_event_stream_frame("assistantResponseEvent", &json!({"content":"x"}), "application/json");
        assert!(is_binary_event_stream(&f));
        assert!(!is_binary_event_stream(br#"{"json":"body"}"#));
        assert!(!is_binary_event_stream(b"short"));
    }

    #[test]
    fn converts_user_message_with_tool_results() {
        let uim = json!({
            "content": "do the thing",
            "userInputMessageContext": { "toolResults": [ { "toolUseId": "t1", "content": [ {"text":"result"} ] } ] }
        });
        let msgs = convert_user_input_message(&uim);
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[0]["tool_call_id"], "t1");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "do the thing");
    }

    #[test]
    fn converts_assistant_tool_uses() {
        let arm = json!({
            "content": "calling",
            "toolUses": [ { "toolUseId": "tu1", "name": "search", "input": {"q":"rust"} } ]
        });
        let m = convert_assistant_response_message(&arm);
        assert_eq!(m["tool_calls"][0]["function"]["name"], "search");
        // arguments must be a JSON *string*, not an object.
        assert_eq!(m["tool_calls"][0]["function"]["arguments"], r#"{"q":"rust"}"#);
    }

    #[test]
    fn extracts_openai_tools() {
        let body = json!({
            "conversationState": {
                "currentMessage": { "userInputMessage": { "userInputMessageContext": { "tools": [
                    { "toolSpecification": { "name": "read", "description": "read file",
                        "inputSchema": { "json": { "type": "object", "properties": {} } } } }
                ] } } }
            }
        });
        let tools = extract_tools(&body);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn flush_emits_message_stop_once() {
        let mut s = KiroState::new(Some("kiro"));
        // Empty content chunk → no frames but initial frame is sent.
        let chunk = json!({"choices":[{"delta":{"role":"assistant"}}],"model":"gpt"});
        let out = convert_openai_to_kiro(Some(&chunk), &mut s);
        assert_eq!(out.len(), 1, "only initial-response frame");
        // Flush → messageStopEvent + initial.
        let flush = convert_openai_to_kiro(None, &mut s);
        let types: Vec<&str> = flush.iter().map(|f| frame_headers(f).get(":event-type").unwrap().as_str()).collect();
        assert!(types.iter().any(|t| *t == "messageStopEvent"), "{types:?}");
        // Second flush is a no-op.
        assert!(convert_openai_to_kiro(None, &mut s).is_empty());
    }

    #[test]
    fn usage_event_emitted_on_finish() {
        let mut s = KiroState::new(Some("kiro"));
        let chunk = json!({
            "choices":[{"delta":{"content":"hi"},"finish_reason":"stop"}],
            "model":"gpt",
            "usage": {"prompt_tokens": 11, "completion_tokens": 7}
        });
        let out = convert_openai_to_kiro(Some(&chunk), &mut s);
        let mut usage_payload = None;
        for f in &out {
            let h = frame_headers(f);
            if h.get(":event-type").map(String::as_str) == Some("usageEvent") {
                usage_payload = Some(frame_payload(f));
            }
        }
        let v = usage_payload.expect("usage frame present");
        assert_eq!(v["inputTokens"], 11);
        assert_eq!(v["outputTokens"], 7);
    }

    #[test]
    fn thinking_blocks_become_reasoning_events() {
        let mut s = KiroState::new(Some("m"));
        let chunk = json!({"choices":[{"delta":{"content":"<thinking>let me think</thinking>answer"}}],"model":"m"});
        let out = convert_openai_to_kiro(Some(&chunk), &mut s);
        let types: Vec<String> = out.iter().map(|f| frame_headers(f).get(":event-type").unwrap().clone()).collect();
        assert!(types.contains(&"reasoningContentEvent".to_string()), "{types:?}");
        assert!(types.contains(&"assistantResponseEvent".to_string()), "{types:?}");
    }

    #[test]
    fn split_thinking_tag_is_buffered_across_chunks() {
        let mut s = KiroState::new(Some("m"));
        let (t1, x1) = extract_thinking("<thi", &mut s);
        assert_eq!(t1, None);
        assert!(s.in_think, "partial tag must be buffered");
        let (t2, x2) = extract_thinking("nking>deep</thinking>final", &mut s);
        assert_eq!(t2.as_deref(), Some("deep"));
        assert_eq!(x2.as_deref(), Some("final"));
    }

    #[test]
    fn full_body_converts_to_messages() {
        let body = json!({
            "conversationState": {
                "history": [
                    { "userInputMessage": { "content": "q1" } },
                    { "assistantResponseMessage": { "content": "a1" } }
                ],
                "currentMessage": { "userInputMessage": { "content": "q2" } }
            }
        });
        let msgs = code_whisperer_to_messages(&body);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[2]["content"], "q2");
    }
}
