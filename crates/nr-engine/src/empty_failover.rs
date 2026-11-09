//! Detect a streamed completion that finished without ever producing usable
//! content, so `handle_combo_chat` can fall through to the next candidate
//! instead of returning a 200 that carries nothing.
//! Port of `open-sse/services/emptyFailover.js`.
//!
//! Why this exists: Gemini aborts a generation with finishReason
//! MALFORMED_FUNCTION_CALL (the model attempts a native function call the API
//! can't serialize). Upstream still returns HTTP 200 and streams a terminal
//! frame with empty content, which the translator maps to finish_reason
//! "stop". The combo's HTTP-status check then logs "succeeded" and the empty
//! response reaches the client.
//!
//! The signal is STRUCTURAL, not an enum: "the stream ended and no content
//! frame was ever released". That covers MALFORMED_FUNCTION_CALL, any future
//! Gemini abort reason, and a stream that simply dies with no reason
//! vocabulary to keep in sync. The finish-reason enum is used only in the
//! inverse: to NOT retry a legitimately-empty content-filter block.

use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use nr_core::http::{BodyStream, UpstreamResponse};
use nr_translator::schema::{
    claude_blocks, claude_deltas, claude_events, gemini_finish, openai_finish,
    responses_events, responses_items, responses_statuses,
};
use serde_json::{json, Value};

/// Helper trait to accept `Option<&str>`, `&str`, or `()` as a format specifier.
pub trait IntoFormat {
    fn into_format(&self) -> Option<&str>;
}

impl IntoFormat for Option<&str> {
    fn into_format(&self) -> Option<&str> {
        *self
    }
}

impl IntoFormat for &str {
    fn into_format(&self) -> Option<&str> {
        Some(self)
    }
}

impl IntoFormat for () {
    fn into_format(&self) -> Option<&str> {
        None
    }
}

/// `NO_RETRY_EMPTY_FINISH`: an empty stream with this finish is legitimate.
/// Never retry content filtering / safety / recitation / blocklist.
fn is_no_retry_finish_reason(reason: &str) -> bool {
    let trimmed = reason.trim();
    trimmed.eq_ignore_ascii_case(openai_finish::CONTENT_FILTER)
        || trimmed.eq_ignore_ascii_case("refusal")
        || trimmed.eq_ignore_ascii_case(gemini_finish::SAFETY)
        || trimmed.eq_ignore_ascii_case(gemini_finish::RECITATION)
        || trimmed.eq_ignore_ascii_case(gemini_finish::BLOCKLIST)
        || trimmed.eq_ignore_ascii_case(gemini_finish::PROHIBITED_CONTENT)
}

fn non_empty_string(value: Option<&Value>) -> bool {
    value.and_then(Value::as_str).map(|s| !s.is_empty()).unwrap_or(false)
}

/// `deltaHasClientContent`: checks whether a chunk / delta carries assistant output
/// the client can consume, across OpenAI, Claude, Gemini, and Responses formats.
///
/// Deliberately NOT content:
/// - `delta.role` - 9router emits `{role:"assistant"}` on the first frame
///   regardless of content; treating it as content would defeat the check.
/// - `reasoning_content` / `delta.reasoning` / `thinking_delta` / `thought` - a
///   think-then-abort produces reasoning only, which must still fall through.
pub fn delta_has_client_content(chunk: &Value, format: impl IntoFormat) -> bool {
    let fmt = format.into_format().unwrap_or("").to_ascii_lowercase();

    // Format-targeted inspection if specified
    if fmt == "gemini" || fmt.contains("gemini") {
        return gemini_chunk_has_content(chunk);
    }
    if fmt == "claude" || fmt == "anthropic" {
        return claude_chunk_has_content(chunk);
    }
    if fmt == "responses" || fmt.contains("responses") {
        return responses_chunk_has_content(chunk);
    }
    if fmt == "openai" || fmt.contains("chat") {
        return openai_chunk_has_content(chunk);
    }

    // Auto-detect based on payload structure
    if chunk.get("candidates").is_some() {
        return gemini_chunk_has_content(chunk);
    }
    if let Some(event_type) = chunk.get("type").and_then(Value::as_str) {
        if event_type.starts_with(responses_events::PREFIX) {
            return responses_chunk_has_content(chunk);
        }
        if event_type.starts_with("content_block_") || event_type.starts_with("message_") {
            return claude_chunk_has_content(chunk);
        }
    }
    if chunk.get("choices").is_some() {
        return openai_chunk_has_content(chunk);
    }

    // Check individual delta / block shapes
    gemini_chunk_has_content(chunk)
        || claude_chunk_has_content(chunk)
        || responses_chunk_has_content(chunk)
        || openai_chunk_has_content(chunk)
}

fn openai_chunk_has_content(chunk: &Value) -> bool {
    let Some(obj) = chunk.as_object() else { return false };

    // If chunk wraps `choices`, inspect each choice delta
    if let Some(choices) = obj.get("choices").and_then(Value::as_array) {
        return choices.iter().any(|c| {
            let delta = c.get("delta").or_else(|| c.get("message")).unwrap_or(&Value::Null);
            openai_chunk_has_content(delta)
        });
    }

    if let Some(content) = obj.get("content") {
        match content {
            Value::String(s) if !s.is_empty() => return true,
            Value::Array(items) if !items.is_empty() => return true,
            _ => {}
        }
    }
    if let Some(Value::Array(calls)) = obj.get("tool_calls") {
        if !calls.is_empty() {
            return true;
        }
    }
    if obj.get("function_call").is_some() {
        return true;
    }
    // OpenAI "Responses" style / non-chat deltas occasionally use output_text.
    if let Some(Value::String(text)) = obj.get("output_text") {
        if !text.is_empty() {
            return true;
        }
    }
    false
}

fn claude_chunk_has_content(chunk: &Value) -> bool {
    let Some(obj) = chunk.as_object() else { return false };

    if let Some(delta) = obj.get("delta") {
        let kind = delta.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == claude_deltas::TEXT && non_empty_string(delta.get("text")) {
            return true;
        }
        if kind == claude_deltas::INPUT_JSON && non_empty_string(delta.get("partial_json")) {
            return true;
        }
        if kind == claude_blocks::THINKING {
            return false;
        }
    }

    if let Some(block) = obj.get("content_block") {
        let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == claude_blocks::TOOL_USE || (kind == claude_blocks::TEXT && non_empty_string(block.get("text"))) {
            return true;
        }
    }

    let kind = obj.get("type").and_then(Value::as_str).unwrap_or("");
    if kind == claude_deltas::TEXT && non_empty_string(obj.get("text")) {
        return true;
    }
    if kind == claude_deltas::INPUT_JSON && non_empty_string(obj.get("partial_json")) {
        return true;
    }

    false
}

fn gemini_chunk_has_content(chunk: &Value) -> bool {
    let Some(obj) = chunk.as_object() else { return false };

    // Nested candidates array
    if let Some(candidates) = obj.get("candidates").and_then(Value::as_array) {
        return candidates.iter().any(gemini_chunk_has_content);
    }

    // Nested content with parts
    if let Some(content) = obj.get("content") {
        if gemini_chunk_has_content(content) {
            return true;
        }
    }

    // Parts array
    if let Some(parts) = obj.get("parts").and_then(Value::as_array) {
        return parts.iter().any(gemini_part_has_content);
    }

    gemini_part_has_content(chunk)
}

fn gemini_part_has_content(part: &Value) -> bool {
    let Some(obj) = part.as_object() else { return false };

    // Thought parts are reasoning, NOT client content
    if obj.get("thought").and_then(Value::as_bool).unwrap_or(false) {
        return false;
    }

    if non_empty_string(obj.get("text")) {
        return true;
    }
    if obj.get("functionCall").or_else(|| obj.get("function_call")).is_some() {
        return true;
    }
    if obj.get("inlineData").or_else(|| obj.get("inline_data")).is_some() {
        return true;
    }

    false
}

fn responses_chunk_has_content(chunk: &Value) -> bool {
    let Some(obj) = chunk.as_object() else { return false };
    let event_type = obj.get("type").and_then(Value::as_str).unwrap_or("");

    match event_type {
        responses_events::OUTPUT_TEXT_DELTA
        | responses_events::FUNCTION_CALL_ARGUMENTS_DELTA
        | responses_events::CUSTOM_TOOL_CALL_INPUT_DELTA => {
            non_empty_string(obj.get("delta"))
        }
        responses_events::OUTPUT_TEXT_DONE => {
            non_empty_string(obj.get("text"))
        }
        responses_events::OUTPUT_ITEM_ADDED | responses_events::OUTPUT_ITEM_DONE => {
            let item_type = obj.get("item").and_then(|i| i.get("type")).and_then(Value::as_str).unwrap_or("");
            item_type == responses_items::FUNCTION_CALL || item_type == responses_items::CUSTOM_TOOL_CALL
        }
        _ => {
            // Direct delta check
            non_empty_string(obj.get("delta"))
        }
    }
}

/// What one SSE line says about the stream.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StreamInspection {
    pub content: bool,
    pub finish_reason: Option<String>,
}

/// `inspectClaudeEvent`: Anthropic Messages event.
fn inspect_claude_event(obj: &Value) -> StreamInspection {
    let mut out = StreamInspection::default();
    match obj.get("type").and_then(Value::as_str).unwrap_or("") {
        claude_events::CONTENT_BLOCK_DELTA => {
            let Some(delta) = obj.get("delta") else { return out };
            let kind = delta.get("type").and_then(Value::as_str).unwrap_or("");
            if kind == claude_deltas::TEXT && non_empty_string(delta.get("text")) {
                out.content = true;
            }
            if kind == claude_deltas::INPUT_JSON && non_empty_string(delta.get("partial_json")) {
                out.content = true;
            }
        }
        claude_events::CONTENT_BLOCK_START => {
            let Some(block) = obj.get("content_block") else { return out };
            let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
            if kind == claude_blocks::TOOL_USE || (kind == claude_blocks::TEXT && non_empty_string(block.get("text"))) {
                out.content = true;
            }
        }
        claude_events::MESSAGE_DELTA => {
            // refusal -> content_filter (the translator renders content_filter
            // as refusal for Claude clients).
            if let Some(stop_reason) = obj.get("delta").and_then(|d| d.get("stop_reason")).filter(|v| !v.is_null()) {
                let stop = stop_reason.as_str().unwrap_or("");
                out.finish_reason = Some(
                    nr_translator::concerns::finish_reason::to_openai_finish(Some(stop), "claude").to_string(),
                );
            }
        }
        _ => {}
    }
    out
}

/// `inspectResponsesEvent`: OpenAI Responses event.
fn inspect_responses_event(obj: &Value) -> StreamInspection {
    let mut out = StreamInspection::default();
    let event_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
    match event_type {
        responses_events::OUTPUT_TEXT_DELTA
        | responses_events::FUNCTION_CALL_ARGUMENTS_DELTA
        | responses_events::CUSTOM_TOOL_CALL_INPUT_DELTA => {
            out.content = non_empty_string(obj.get("delta"));
        }
        responses_events::OUTPUT_TEXT_DONE => {
            out.content = non_empty_string(obj.get("text"));
        }
        responses_events::OUTPUT_ITEM_ADDED | responses_events::OUTPUT_ITEM_DONE => {
            let item_type = obj.get("item").and_then(|i| i.get("type")).and_then(Value::as_str).unwrap_or("");
            out.content = item_type == responses_items::FUNCTION_CALL || item_type == responses_items::CUSTOM_TOOL_CALL;
        }
        responses_events::COMPLETED => {
            out.finish_reason = Some(openai_finish::STOP.to_string());
        }
        responses_events::INCOMPLETE => {
            let reason = obj
                .get("response")
                .and_then(|r| r.get("incomplete_details"))
                .and_then(|d| d.get("reason"))
                .and_then(Value::as_str)
                .unwrap_or("");
            out.finish_reason = Some(
                match reason {
                    "content_filter" => openai_finish::CONTENT_FILTER,
                    "max_output_tokens" => openai_finish::LENGTH,
                    _ => responses_statuses::INCOMPLETE,
                }
                .to_string(),
            );
        }
        responses_events::FAILED => {
            out.finish_reason = Some(responses_statuses::FAILED.to_string());
        }
        _ => {}
    }
    out
}

/// Gemini streaming candidate event.
fn inspect_gemini_event(obj: &Value) -> StreamInspection {
    let mut out = StreamInspection::default();
    let Some(candidates) = obj.get("candidates").and_then(Value::as_array) else {
        return out;
    };
    for cand in candidates {
        if gemini_chunk_has_content(cand) {
            out.content = true;
        }
        if let Some(finish) = cand.get("finishReason").or_else(|| cand.get("finish_reason")).and_then(Value::as_str) {
            if !finish.is_empty() {
                let mapped = match finish {
                    gemini_finish::STOP => openai_finish::STOP,
                    gemini_finish::MAX_TOKENS => openai_finish::LENGTH,
                    gemini_finish::SAFETY
                    | gemini_finish::RECITATION
                    | gemini_finish::BLOCKLIST
                    | gemini_finish::PROHIBITED_CONTENT => openai_finish::CONTENT_FILTER,
                    other => other,
                };
                out.finish_reason = Some(mapped.to_string());
            }
        }
    }
    out
}

/// Parse one SSE payload line: strips `data:` and ignores `[DONE]`.
fn parse_sse_payload(line: &str) -> Option<Value> {
    let trimmed = line.trim_start();
    let data = trimmed.strip_prefix("data:")?.trim();
    if data == "[DONE]" {
        return None;
    }
    serde_json::from_str(data).ok()
}

/// Inspect one complete client-format SSE line: does it carry content and/or a
/// finish reason? Non-data lines and `[DONE]` carry nothing.
pub fn inspect_sse_line(line: &str, format: impl IntoFormat) -> StreamInspection {
    let Some(parsed) = parse_sse_payload(line) else {
        return StreamInspection::default();
    };

    let fmt = format.into_format().unwrap_or("").to_ascii_lowercase();

    if fmt == "gemini" || fmt.contains("gemini") || parsed.get("candidates").is_some() {
        return inspect_gemini_event(&parsed);
    }

    let is_typed = parsed.get("type").and_then(Value::as_str).is_some() && parsed.get("choices").is_none();
    if is_typed {
        let event_type = parsed.get("type").and_then(Value::as_str).unwrap_or("");
        return if event_type.starts_with(responses_events::PREFIX) {
            inspect_responses_event(&parsed)
        } else {
            inspect_claude_event(&parsed)
        };
    }

    let mut out = StreamInspection::default();
    if let Some(choices) = parsed.get("choices").and_then(Value::as_array) {
        for choice in choices {
            let delta = choice.get("delta").or_else(|| choice.get("message")).unwrap_or(&Value::Null);
            if delta_has_client_content(delta, format.into_format()) {
                out.content = true;
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str).filter(|r| !r.is_empty()) {
                out.finish_reason = Some(reason.to_string());
            }
        }
    }
    out
}

/// Running decision while scanning a buffered stream.
#[derive(Clone, Debug, Default)]
pub struct ScanState {
    /// Latches true on the first content frame.
    pub saw_content: bool,
    pub finish_reason: Option<String>,
    /// Trailing fragment: a line still being received, for the next chunk.
    pub pending: String,
}

impl ScanState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// `scanSSEBuffer`: scan the COMPLETE lines of a decoded SSE buffer and update
/// the running decision. The trailing fragment is not parsed; it is returned in
/// [`ScanState::pending`] for the caller to prepend to the next chunk.
pub fn scan_sse_buffer(text: &str, state: &mut ScanState, format: impl IntoFormat) {
    let fmt = format.into_format();
    let mut lines: Vec<&str> = text.split('\n').collect();
    state.pending = lines.pop().unwrap_or("").to_string();
    for line in lines {
        let info = inspect_sse_line(line, fmt);
        if info.content {
            state.saw_content = true;
        }
        if let Some(reason) = info.finish_reason {
            state.finish_reason = Some(reason);
        }
    }
}

/// True when a finished, content-less stream should NOT be retried because the
/// emptiness is legitimate and would recur on every candidate.
pub fn is_no_retry_finish(finish_reason: Option<&str>) -> bool {
    match finish_reason {
        Some(reason) => is_no_retry_finish_reason(reason),
        None => false,
    }
}

/// `is_terminal_empty_stream`: true when a finished empty stream should trigger failover.
/// Returns true for abortive terminations (e.g. Gemini `MALFORMED_FUNCTION_CALL`, empty `stop`,
/// or unexpected stream close), and false for legitimate blocks (content filter, safety, refusal).
pub fn is_terminal_empty_stream(finish_reason: Option<&str>, format: impl IntoFormat) -> bool {
    let _ = format;
    !is_no_retry_finish(finish_reason)
}

/// Outcome of peeking a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeekOutcome {
    /// A content frame arrived (or the deadline fired: fail-open).
    Content,
    /// The stream ended without any content frame.
    Empty,
}

/// Decision for a finished stream: should the combo fall through?
pub fn should_fail_over(saw_content: bool, finish_reason: Option<&str>) -> bool {
    !saw_content && !is_no_retry_finish(finish_reason)
}

/// Default peek deadline (`EMPTY_PEEK_DEADLINE_MS` in runtimeConfig.js).
pub fn empty_peek_deadline_ms() -> u64 {
    nr_data::runtime_u64("EMPTY_PEEK_DEADLINE_MS").max(1)
}

/// Result of `peek_stream_for_content`.
pub struct PeekedStream {
    pub outcome: PeekOutcome,
    pub timed_out: bool,
    pub finish_reason: Option<String>,
    pub head: Vec<Bytes>,
    pub rest: BodyStream,
}

impl PeekedStream {
    pub fn reconstruct(self) -> BodyStream {
        reconstruct_stream(self.head, self.rest)
    }
}

/// Reconstruct a `BodyStream` from buffered head chunks and the remaining stream.
pub fn reconstruct_stream(head: Vec<Bytes>, rest: BodyStream) -> BodyStream {
    Box::pin(futures::stream::iter(head.into_iter().map(Ok)).chain(rest))
}

/// Peek a streaming Response body: read frames until the first content frame (-> Content),
/// the stream ends (-> Empty), or the deadline elapses (-> Content, fail-open).
pub async fn peek_stream_for_content(
    mut body: BodyStream,
    deadline_ms: Option<u64>,
    format: impl IntoFormat,
) -> PeekedStream {
    let fmt = format.into_format();
    let timeout_ms = match deadline_ms {
        Some(ms) if ms > 0 => ms,
        _ => empty_peek_deadline_ms(),
    };

    let mut head = Vec::new();
    let mut state = ScanState::new();
    let mut text_buf = String::new();

    loop {
        let chunk_res = match tokio::time::timeout(Duration::from_millis(timeout_ms), body.next()).await {
            Ok(item) => item,
            Err(_) => {
                // Fail-open: commit whatever we have.
                return PeekedStream {
                    outcome: PeekOutcome::Content,
                    timed_out: true,
                    finish_reason: state.finish_reason,
                    head,
                    rest: body,
                };
            }
        };

        match chunk_res {
            Some(Ok(bytes)) => {
                head.push(bytes.clone());
                let text = String::from_utf8_lossy(&bytes);
                text_buf.push_str(&text);
                scan_sse_buffer(&text_buf, &mut state, fmt);
                text_buf = state.pending.clone();
                if state.saw_content {
                    return PeekedStream {
                        outcome: PeekOutcome::Content,
                        timed_out: false,
                        finish_reason: state.finish_reason,
                        head,
                        rest: body,
                    };
                }
            }
            Some(Err(err)) => {
                let outcome = if state.saw_content {
                    PeekOutcome::Content
                } else {
                    PeekOutcome::Empty
                };
                let rest = Box::pin(futures::stream::once(async move { Err(err) }).chain(body));
                return PeekedStream {
                    outcome,
                    timed_out: false,
                    finish_reason: state.finish_reason,
                    head,
                    rest,
                };
            }
            None => {
                if !text_buf.is_empty() {
                    let final_line = format!("{text_buf}\n");
                    scan_sse_buffer(&final_line, &mut state, fmt);
                }
                let outcome = if state.saw_content {
                    PeekOutcome::Content
                } else {
                    PeekOutcome::Empty
                };
                return PeekedStream {
                    outcome,
                    timed_out: false,
                    finish_reason: state.finish_reason,
                    head,
                    rest: body,
                };
            }
        }
    }
}

/// Result of peeking a combo response.
pub struct PeekedComboResponse {
    pub outcome: PeekOutcome,
    pub finish_reason: Option<String>,
    pub timed_out: bool,
    pub response: UpstreamResponse,
}

/// Peek a 2xx combo response for real content. Returns `PeekedComboResponse`
/// with a faithfully replayed stream.
pub async fn peek_combo_response(
    response: UpstreamResponse,
    format: impl IntoFormat,
) -> PeekedComboResponse {
    let status = response.status;
    let headers = response.headers.clone();
    let peeked = peek_stream_for_content(response.body, None, format).await;
    let outcome = peeked.outcome;
    let finish_reason = peeked.finish_reason;
    let timed_out = peeked.timed_out;
    let stream = reconstruct_stream(peeked.head, peeked.rest);
    let rebuilt = UpstreamResponse::from_stream(status, headers, stream);
    PeekedComboResponse {
        outcome,
        finish_reason,
        timed_out,
        response: rebuilt,
    }
}

/// `classify_body`: for non-streaming or fully buffered body inspection.
pub fn classify_body(body: &str, format: impl IntoFormat) -> (PeekOutcome, Option<String>) {
    let fmt = format.into_format();
    let mut state = ScanState::new();
    let full = if body.ends_with('\n') { body.to_string() } else { format!("{body}\n") };
    scan_sse_buffer(&full, &mut state, fmt);
    let outcome = if state.saw_content { PeekOutcome::Content } else { PeekOutcome::Empty };
    (outcome, state.finish_reason)
}

/// Check non-streaming 200 JSON responses for zero usable content.
pub fn is_zero_content_json(data: &Value) -> bool {
    let Some(obj) = data.as_object() else { return false };

    // OpenAI chat format
    if let Some(choices) = obj.get("choices").and_then(Value::as_array) {
        if choices.is_empty() {
            return true;
        }
        let choice = &choices[0];
        let msg = choice.get("message").or_else(|| choice.get("delta")).unwrap_or(&Value::Null);
        let has_content = match msg.get("content") {
            Some(Value::String(s)) => !s.trim().is_empty(),
            Some(Value::Array(arr)) => !arr.is_empty(),
            _ => false,
        };
        let has_tool_calls = msg.get("tool_calls").and_then(Value::as_array).map(|a| !a.is_empty()).unwrap_or(false)
            || msg.get("function_call").is_some();
        let has_reasoning = msg.get("reasoning_content").and_then(Value::as_str).map(|s| !s.trim().is_empty()).unwrap_or(false);
        let finish = choice.get("finish_reason").and_then(Value::as_str).unwrap_or("");
        let is_normal_finish = matches!(finish, "stop" | "length" | "tool_calls" | "function_call");

        if !has_content && !has_tool_calls && !has_reasoning && (!is_normal_finish || finish == "in_progress" || finish == "incomplete") {
            return true;
        }
    }

    // Anthropic messages format
    if let Some(content) = obj.get("content").and_then(Value::as_array) {
        let has_content = content.iter().any(|c| {
            let kind = c.get("type").and_then(Value::as_str).unwrap_or("");
            (kind == "text" && c.get("text").and_then(Value::as_str).map(|s| !s.trim().is_empty()).unwrap_or(false))
                || kind == "tool_use"
        });
        let stop_reason = obj.get("stop_reason").and_then(Value::as_str).unwrap_or("");
        if !has_content && (stop_reason.is_empty() || stop_reason == "in_progress" || stop_reason == "incomplete") {
            return true;
        }
    }

    // Gemini format
    if let Some(candidates) = obj.get("candidates").and_then(Value::as_array) {
        if candidates.is_empty() {
            return true;
        }
        let cand = &candidates[0];
        let parts = cand.get("content").and_then(|c| c.get("parts")).and_then(Value::as_array);
        let has_content = parts.map(|p| p.iter().any(|part| {
            let is_thought = part.get("thought").and_then(Value::as_bool).unwrap_or(false);
            if is_thought {
                return false;
            }
            let has_text = part.get("text").and_then(Value::as_str).map(|s| !s.is_empty()).unwrap_or(false);
            let has_fc = part.get("functionCall").or_else(|| part.get("function_call")).is_some();
            has_text || has_fc
        })).unwrap_or(false);
        let finish = cand.get("finishReason").or_else(|| cand.get("finish_reason")).and_then(Value::as_str).unwrap_or("");
        if !has_content && (finish == gemini_finish::MALFORMED_FUNCTION_CALL || finish == gemini_finish::STOP) {
            return true;
        }
    }

    false
}

/// A readable summary of a scan, used by the combo path and by `/api` callers.
pub fn scan_summary(state: &ScanState) -> Value {
    json!({
        "sawContent": state.saw_content,
        "finishReason": state.finish_reason,
        "shouldFailOver": should_fail_over(state.saw_content, state.finish_reason.as_deref()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_stream(frames: &[&str]) -> BodyStream {
        let chunks: Vec<Bytes> = frames.iter().map(|f| Bytes::from(f.to_string())).collect();
        Box::pin(futures::stream::iter(chunks.into_iter().map(Ok)))
    }

    async fn drain_stream(mut stream: BodyStream) -> String {
        let mut out = String::new();
        while let Some(chunk) = stream.next().await {
            let bytes = chunk.expect("chunk ok");
            out.push_str(&String::from_utf8_lossy(&bytes));
        }
        out
    }

    // ─── Format 1: OpenAI Tests ──────────────────────────────────────────────

    #[test]
    fn openai_content_variants() {
        assert!(delta_has_client_content(&json!({"content": "hi"}), "openai"));
        assert!(delta_has_client_content(&json!({"content": [{"type": "text"}]}), "openai"));
        assert!(delta_has_client_content(&json!({"tool_calls": [{"id": "t"}]}), "openai"));
        assert!(delta_has_client_content(&json!({"output_text": "hi"}), "openai"));

        // Role alone is not content (9router emits it on frame one).
        assert!(!delta_has_client_content(&json!({"role": "assistant"}), "openai"));
        // Reasoning alone must still fall through.
        assert!(!delta_has_client_content(&json!({"reasoning_content": "thinking"}), "openai"));
        assert!(!delta_has_client_content(&json!({"content": ""}), "openai"));
        assert!(!delta_has_client_content(&json!({"content": []}), "openai"));
        assert!(!delta_has_client_content(&json!({"tool_calls": []}), "openai"));
        assert!(!delta_has_client_content(&Value::Null, "openai"));
    }

    #[test]
    fn openai_chunk_is_content() {
        let line = r#"data: {"choices":[{"delta":{"content":"hello"},"finish_reason":null}]}"#;
        let info = inspect_sse_line(line, "openai");
        assert!(info.content);
        assert_eq!(info.finish_reason, None);
    }

    #[test]
    fn openai_empty_terminal_chunk_is_not_content() {
        let line = r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#;
        let info = inspect_sse_line(line, "openai");
        assert!(!info.content);
        assert_eq!(info.finish_reason.as_deref(), Some("stop"));
        assert!(is_terminal_empty_stream(info.finish_reason.as_deref(), "openai"));
        assert!(should_fail_over(false, info.finish_reason.as_deref()));
    }

    // ─── Format 2: Claude Tests ──────────────────────────────────────────────

    #[test]
    fn claude_events() {
        let text = r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}"#;
        assert!(inspect_sse_line(text, "claude").content);

        let tool_json = r#"data: {"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{"}}"#;
        assert!(inspect_sse_line(tool_json, "claude").content);

        let tool_block = r#"data: {"type":"content_block_start","content_block":{"type":"tool_use","id":"t"}}"#;
        assert!(inspect_sse_line(tool_block, "claude").content);

        let text_block = r#"data: {"type":"content_block_start","content_block":{"type":"text","text":"x"}}"#;
        assert!(inspect_sse_line(text_block, "claude").content);

        let empty_block = r#"data: {"type":"content_block_start","content_block":{"type":"text","text":""}}"#;
        assert!(!inspect_sse_line(empty_block, "claude").content);

        let thinking = r#"data: {"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"hmm"}}"#;
        assert!(!inspect_sse_line(thinking, "claude").content);

        let end_turn = r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#;
        let end_info = inspect_sse_line(end_turn, "claude");
        assert!(!end_info.content);
        assert_eq!(end_info.finish_reason.as_deref(), Some("stop"));
        assert!(is_terminal_empty_stream(end_info.finish_reason.as_deref(), "claude"));

        let refusal = r#"data: {"type":"message_delta","delta":{"stop_reason":"refusal"}}"#;
        let info = inspect_sse_line(refusal, "claude");
        assert!(!info.content);
        assert_eq!(info.finish_reason.as_deref(), Some(openai_finish::CONTENT_FILTER));
        assert!(!is_terminal_empty_stream(info.finish_reason.as_deref(), "claude"));
        assert!(!should_fail_over(false, info.finish_reason.as_deref()));
    }

    // ─── Format 3: Gemini Tests ──────────────────────────────────────────────

    #[test]
    fn gemini_events_and_content() {
        let content_line = r#"data: {"candidates":[{"content":{"parts":[{"text":"hello world"}]}}]}"#;
        let c_info = inspect_sse_line(content_line, "gemini");
        assert!(c_info.content);
        assert_eq!(c_info.finish_reason, None);

        let tool_line = r#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"calc","args":{}}}]}}]}"#;
        assert!(inspect_sse_line(tool_line, "gemini").content);

        // Thought is reasoning, not content
        let thought_line = r#"data: {"candidates":[{"content":{"parts":[{"text":"thinking...","thought":true}]}}]}"#;
        assert!(!inspect_sse_line(thought_line, "gemini").content);

        // Gemini abort with MALFORMED_FUNCTION_CALL
        let malformed = r#"data: {"candidates":[{"finishReason":"MALFORMED_FUNCTION_CALL"}]}"#;
        let m_info = inspect_sse_line(malformed, "gemini");
        assert!(!m_info.content);
        assert_eq!(m_info.finish_reason.as_deref(), Some("MALFORMED_FUNCTION_CALL"));
        assert!(is_terminal_empty_stream(m_info.finish_reason.as_deref(), "gemini"));
        assert!(should_fail_over(false, m_info.finish_reason.as_deref()));

        // Gemini empty with STOP
        let empty_stop = r#"data: {"candidates":[{"content":{"parts":[]},"finishReason":"STOP"}]}"#;
        let s_info = inspect_sse_line(empty_stop, "gemini");
        assert!(!s_info.content);
        assert_eq!(s_info.finish_reason.as_deref(), Some("stop"));
        assert!(is_terminal_empty_stream(s_info.finish_reason.as_deref(), "gemini"));

        // Gemini SAFETY filter - do NOT retry
        let safety = r#"data: {"candidates":[{"finishReason":"SAFETY"}]}"#;
        let saf_info = inspect_sse_line(safety, "gemini");
        assert!(!saf_info.content);
        assert_eq!(saf_info.finish_reason.as_deref(), Some("content_filter"));
        assert!(!is_terminal_empty_stream(saf_info.finish_reason.as_deref(), "gemini"));
        assert!(!should_fail_over(false, saf_info.finish_reason.as_deref()));

        // Gemini RECITATION filter - do NOT retry
        let recitation = r#"data: {"candidates":[{"finishReason":"RECITATION"}]}"#;
        let rec_info = inspect_sse_line(recitation, "gemini");
        assert!(!is_terminal_empty_stream(rec_info.finish_reason.as_deref(), "gemini"));

        // Delta helper checks
        assert!(delta_has_client_content(&json!({"candidates":[{"content":{"parts":[{"text":"hi"}]}}]}), "gemini"));
        assert!(!delta_has_client_content(&json!({"candidates":[{"finishReason":"MALFORMED_FUNCTION_CALL"}]}), "gemini"));
        assert!(!delta_has_client_content(&json!({"candidates":[{"content":{"parts":[{"text":"x","thought":true}]}}]}), "gemini"));
    }

    // ─── Format 4: Responses Tests ───────────────────────────────────────────

    #[test]
    fn responses_events() {
        let delta = r#"data: {"type":"response.output_text.delta","delta":"hi"}"#;
        assert!(inspect_sse_line(delta, "responses").content);

        let done = r#"data: {"type":"response.output_text.done","text":"hi"}"#;
        assert!(inspect_sse_line(done, "responses").content);

        let call = r#"data: {"type":"response.output_item.added","item":{"type":"function_call"}}"#;
        assert!(inspect_sse_line(call, "responses").content);

        let message = r#"data: {"type":"response.output_item.added","item":{"type":"message"}}"#;
        assert!(!inspect_sse_line(message, "responses").content);

        let reasoning = r#"data: {"type":"response.output_item.added","item":{"type":"reasoning"}}"#;
        assert!(!inspect_sse_line(reasoning, "responses").content);

        let completed = r#"data: {"type":"response.completed"}"#;
        assert_eq!(inspect_sse_line(completed, "responses").finish_reason.as_deref(), Some(openai_finish::STOP));
        assert!(is_terminal_empty_stream(Some("stop"), "responses"));

        let incomplete = r#"data: {"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#;
        assert_eq!(inspect_sse_line(incomplete, "responses").finish_reason.as_deref(), Some(openai_finish::LENGTH));
        assert!(is_terminal_empty_stream(Some("length"), "responses"));

        let filtered = r#"data: {"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"}}}"#;
        let fil_info = inspect_sse_line(filtered, "responses");
        assert_eq!(fil_info.finish_reason.as_deref(), Some(openai_finish::CONTENT_FILTER));
        assert!(!is_terminal_empty_stream(fil_info.finish_reason.as_deref(), "responses"));

        let failed = r#"data: {"type":"response.failed"}"#;
        assert_eq!(inspect_sse_line(failed, "responses").finish_reason.as_deref(), Some(responses_statuses::FAILED));
        assert!(is_terminal_empty_stream(Some("failed"), "responses"));
    }

    // ─── Common Inspection & Scanning Tests ──────────────────────────────────

    #[test]
    fn non_data_lines_carry_nothing() {
        assert_eq!(inspect_sse_line("event: message_delta", ()), StreamInspection::default());
        assert_eq!(inspect_sse_line("data: [DONE]", ()), StreamInspection::default());
        assert_eq!(inspect_sse_line("", ()), StreamInspection::default());
        assert_eq!(inspect_sse_line("data: not json", ()), StreamInspection::default());
    }

    #[test]
    fn scan_keeps_trailing_fragment_pending() {
        let mut state = ScanState::new();
        scan_sse_buffer("data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\ndata: {\"choi", &mut state, ());
        assert!(state.saw_content);
        assert_eq!(state.pending, "data: {\"choi");

        // The fragment completes on the next chunk.
        let mut next = ScanState { pending: state.pending, ..ScanState::new() };
        next.saw_content = state.saw_content;
        scan_sse_buffer(&format!("{}ces\":[{{\"delta\":{{}}}}]}}\n", next.pending), &mut next, ());
        assert!(next.saw_content);
        assert_eq!(next.pending, "");
    }

    #[test]
    fn scan_latches_content_across_chunks() {
        let mut state = ScanState::new();
        scan_sse_buffer(
            "data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n",
            &mut state,
            (),
        );
        assert!(state.saw_content);
        assert_eq!(state.finish_reason.as_deref(), Some("stop"));
        assert!(!should_fail_over(state.saw_content, state.finish_reason.as_deref()));
    }

    #[test]
    fn classify_complete_body() {
        let body = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let (outcome, reason) = classify_body(body, ());
        assert_eq!(outcome, PeekOutcome::Empty);
        assert_eq!(reason.as_deref(), Some("stop"));
        assert!(should_fail_over(false, reason.as_deref()));

        let content = "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\ndata: [DONE]\n\n";
        assert_eq!(classify_body(content, ()).0, PeekOutcome::Content);
    }

    #[test]
    fn content_filter_is_never_retried() {
        assert!(is_no_retry_finish(Some("content_filter")));
        assert!(is_no_retry_finish(Some(" Content_Filter ")));
        assert!(is_no_retry_finish(Some("refusal")));
        assert!(is_no_retry_finish(Some("SAFETY")));
        assert!(is_no_retry_finish(Some("RECITATION")));
        assert!(!is_no_retry_finish(Some("stop")));
        assert!(!is_no_retry_finish(Some("MALFORMED_FUNCTION_CALL")));
        assert!(!is_no_retry_finish(None));
    }

    #[test]
    fn summary_flags_failover() {
        let state = ScanState { saw_content: false, finish_reason: Some("length".into()), pending: String::new() };
        let summary = scan_summary(&state);
        assert_eq!(summary["sawContent"], false);
        assert_eq!(summary["shouldFailOver"], true);
        assert_eq!(summary["finishReason"], "length");
    }

    #[test]
    fn peek_deadline_is_positive() {
        assert!(empty_peek_deadline_ms() > 0);
    }

    // ─── Streaming Peek Tests ────────────────────────────────────────────────

    #[tokio::test]
    async fn peek_openai_empty_stop_yields_empty() {
        let role = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n";
        let stop = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
        let done = "data: [DONE]\n\n";
        let stream = make_stream(&[role, stop, done]);

        let peeked = peek_stream_for_content(stream, Some(100), "openai").await;
        assert_eq!(peeked.outcome, PeekOutcome::Empty);
        assert_eq!(peeked.finish_reason.as_deref(), Some("stop"));
        assert!(!peeked.timed_out);

        // Reconstruct preserves all original frames
        let drained = drain_stream(peeked.reconstruct()).await;
        assert_eq!(drained, format!("{role}{stop}{done}"));
    }

    #[tokio::test]
    async fn peek_openai_content_yields_content() {
        let role = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n";
        let content = "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n";
        let done = "data: [DONE]\n\n";
        let stream = make_stream(&[role, content, done]);

        let peeked = peek_stream_for_content(stream, Some(100), "openai").await;
        assert_eq!(peeked.outcome, PeekOutcome::Content);
        assert!(!peeked.timed_out);

        let drained = drain_stream(peeked.reconstruct()).await;
        assert_eq!(drained, format!("{role}{content}{done}"));
    }

    #[tokio::test]
    async fn peek_gemini_malformed_function_call_yields_empty() {
        let malformed = "data: {\"candidates\":[{\"finishReason\":\"MALFORMED_FUNCTION_CALL\"}]}\n\n";
        let stream = make_stream(&[malformed]);

        let peeked = peek_stream_for_content(stream, Some(100), "gemini").await;
        assert_eq!(peeked.outcome, PeekOutcome::Empty);
        assert_eq!(peeked.finish_reason.as_deref(), Some("MALFORMED_FUNCTION_CALL"));
        assert!(is_terminal_empty_stream(peeked.finish_reason.as_deref(), "gemini"));
    }

    #[tokio::test]
    async fn peek_gemini_content_yields_content() {
        let content = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"gemini answer\"}]}}]}\n\n";
        let stream = make_stream(&[content]);

        let peeked = peek_stream_for_content(stream, Some(100), "gemini").await;
        assert_eq!(peeked.outcome, PeekOutcome::Content);
    }

    #[tokio::test]
    async fn peek_claude_content_yields_content() {
        let start = "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n";
        let delta = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n";
        let stop = "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n";
        let stream = make_stream(&[start, delta, stop]);

        let peeked = peek_stream_for_content(stream, Some(100), "claude").await;
        assert_eq!(peeked.outcome, PeekOutcome::Content);
    }

    #[tokio::test]
    async fn peek_responses_content_yields_content() {
        let text_delta = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"resp answer\"}\n\n";
        let completed = "data: {\"type\":\"response.completed\"}\n\n";
        let stream = make_stream(&[text_delta, completed]);

        let peeked = peek_stream_for_content(stream, Some(100), "responses").await;
        assert_eq!(peeked.outcome, PeekOutcome::Content);
    }

    #[tokio::test]
    async fn peek_timeout_fails_open() {
        // Delayed stream that takes longer than the peek deadline
        let chunks = vec![Bytes::from("data: {\"choices\":[{\"delta\":{\"content\":\"delayed\"}}]}\n\n")];
        let stream = Box::pin(futures::stream::unfold((chunks, true), |(mut remaining, is_first)| async move {
            if is_first {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            if remaining.is_empty() {
                None
            } else {
                let chunk = remaining.remove(0);
                Some((Ok(chunk), (remaining, false)))
            }
        }));

        let peeked = peek_stream_for_content(stream, Some(20), ()).await;
        assert_eq!(peeked.outcome, PeekOutcome::Content);
        assert!(peeked.timed_out);

        // Even though it timed out, reconstruct still delivers all bytes without loss
        let drained = drain_stream(peeked.reconstruct()).await;
        assert!(drained.contains("delayed"));
    }

    // ─── Non-streaming JSON tests ────────────────────────────────────────────

    #[test]
    fn zero_content_json_detection() {
        // OpenAI empty choices
        assert!(is_zero_content_json(&json!({"choices": []})));
        assert!(is_zero_content_json(&json!({"choices": [{"message": {"content": ""}, "finish_reason": "in_progress"}]})));
        assert!(!is_zero_content_json(&json!({"choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}]})));
        assert!(!is_zero_content_json(&json!({"choices": [{"message": {"tool_calls": [{"id": "1"}]}, "finish_reason": "tool_calls"}]})));

        // Anthropic
        assert!(is_zero_content_json(&json!({"content": [], "stop_reason": ""})));
        assert!(!is_zero_content_json(&json!({"content": [{"type": "text", "text": "hello"}], "stop_reason": "end_turn"})));

        // Gemini
        assert!(is_zero_content_json(&json!({"candidates": [{"finishReason": "MALFORMED_FUNCTION_CALL"}]})));
        assert!(is_zero_content_json(&json!({"candidates": [{"content": {"parts": []}, "finishReason": "STOP"}]})));
        assert!(!is_zero_content_json(&json!({"candidates": [{"content": {"parts": [{"text": "hello"}]}}]})));
    }
}
