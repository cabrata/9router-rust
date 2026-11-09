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
//! Gemini abort reason, and a stream that simply dies — with no reason
//! vocabulary to keep in sync. The finish-reason enum is used only in the
//! inverse: to NOT retry a legitimately-empty content-filter block.

use serde_json::{json, Value};

use nr_translator::schema::{claude_blocks, claude_deltas, claude_events, openai_finish, responses_events, responses_items, responses_statuses};

/// `NO_RETRY_EMPTY_FINISH`: an empty stream with this finish is legitimate.
fn is_no_retry_finish_reason(reason: &str) -> bool {
    reason.trim().eq_ignore_ascii_case(openai_finish::CONTENT_FILTER)
}

/// `deltaHasClientContent`: OpenAI-chat delta carries assistant output.
///
/// Deliberately NOT content:
/// - `delta.role` — 9router emits `{role:"assistant"}` on the first frame
///   regardless of content; counting it would defeat the whole check.
/// - `reasoning_content` / `delta.reasoning` — a think-then-abort produces
///   reasoning only, which must still fall through.
pub fn delta_has_client_content(delta: &Value) -> bool {
    let Some(obj) = delta.as_object() else { return false };

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
    // OpenAI "Responses" style / non-chat deltas occasionally use output_text.
    if let Some(Value::String(text)) = obj.get("output_text") {
        if !text.is_empty() {
            return true;
        }
    }
    false
}

fn non_empty_string(value: Option<&Value>) -> bool {
    value.and_then(Value::as_str).map(|s| !s.is_empty()).unwrap_or(false)
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
            // A tool_use block is content even before any JSON arrives.
            if kind == claude_blocks::TOOL_USE || (kind == claude_blocks::TEXT && non_empty_string(block.get("text"))) {
                out.content = true;
            }
        }
        claude_events::MESSAGE_DELTA => {
            // refusal → content_filter (the translator renders content_filter
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
///
/// Terminal events: completed → stop; incomplete → content_filter (no-retry)
/// or length by `incomplete_details.reason`; failed → "failed" (retryable).
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

/// `parseSSELine` for one client-format line: `Some(Value)` for a JSON payload,
/// `Some(json!({"__done": true}))` is never returned — `[DONE]` maps to `None`.
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
pub fn inspect_sse_line(line: &str) -> StreamInspection {
    let Some(parsed) = parse_sse_payload(line) else {
        return StreamInspection::default();
    };

    // Both /v1/messages and /v1/responses events are typed; OpenAI chat chunks
    // carry `choices` instead.
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
            if delta_has_client_content(delta) {
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
pub fn scan_sse_buffer(text: &str, state: &mut ScanState) {
    let mut lines: Vec<&str> = text.split('\n').collect();
    state.pending = lines.pop().unwrap_or("").to_string();
    for line in lines {
        let info = inspect_sse_line(line);
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

/// Outcome of peeking a stream.
#[derive(Clone, Debug, PartialEq)]
pub enum PeekOutcome {
    /// A content frame arrived (or the deadline fired: fail-open).
    Content,
    /// The stream ended without any content frame.
    Empty,
}

/// Decision for a finished stream: should the combo fall through?
///
/// `true` for an empty stream whose finish reason is not a legitimate empty
/// termination (content filter / refusal).
pub fn should_fail_over(saw_content: bool, finish_reason: Option<&str>) -> bool {
    !saw_content && !is_no_retry_finish(finish_reason)
}

/// `peekStreamForContent` result for the non-streaming caller, which has the
/// whole body already: classify it.
pub fn classify_body(body: &str) -> (PeekOutcome, Option<String>) {
    let mut state = ScanState::new();
    // The trailing fragment counts as a final line when the body is complete.
    let full = if body.ends_with('\n') { body.to_string() } else { format!("{body}\n") };
    scan_sse_buffer(&full, &mut state);
    let outcome = if state.saw_content { PeekOutcome::Content } else { PeekOutcome::Empty };
    (outcome, state.finish_reason)
}

/// Default peek deadline (`EMPTY_PEEK_DEADLINE_MS` in runtimeConfig.js).
pub fn empty_peek_deadline_ms() -> u64 {
    nr_data::runtime_u64("EMPTY_PEEK_DEADLINE_MS").max(1)
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

    #[test]
    fn openai_content_variants() {
        assert!(delta_has_client_content(&json!({"content": "hi"})));
        assert!(delta_has_client_content(&json!({"content": [{"type": "text"}]})));
        assert!(delta_has_client_content(&json!({"tool_calls": [{"id": "t"}]})));
        assert!(delta_has_client_content(&json!({"output_text": "hi"})));

        // Role alone is not content (9router emits it on frame one).
        assert!(!delta_has_client_content(&json!({"role": "assistant"})));
        // Reasoning alone must still fall through.
        assert!(!delta_has_client_content(&json!({"reasoning_content": "thinking"})));
        assert!(!delta_has_client_content(&json!({"content": ""})));
        assert!(!delta_has_client_content(&json!({"content": []})));
        assert!(!delta_has_client_content(&json!({"tool_calls": []})));
        assert!(!delta_has_client_content(&Value::Null));
    }

    #[test]
    fn openai_chunk_is_content() {
        let line = r#"data: {"choices":[{"delta":{"content":"hello"},"finish_reason":null}]}"#;
        let info = inspect_sse_line(line);
        assert!(info.content);
        assert_eq!(info.finish_reason, None);
    }

    #[test]
    fn openai_empty_terminal_chunk_is_not_content() {
        // This is the Gemini MALFORMED_FUNCTION_CALL shape: 200 + stop, no content.
        let line = r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#;
        let info = inspect_sse_line(line);
        assert!(!info.content);
        assert_eq!(info.finish_reason.as_deref(), Some("stop"));
        assert!(should_fail_over(false, info.finish_reason.as_deref()));
    }

    #[test]
    fn claude_events() {
        let text = r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}"#;
        assert!(inspect_sse_line(text).content);

        let tool_json = r#"data: {"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{"}}"#;
        assert!(inspect_sse_line(tool_json).content);

        let tool_block = r#"data: {"type":"content_block_start","content_block":{"type":"tool_use","id":"t"}}"#;
        assert!(inspect_sse_line(tool_block).content);

        let text_block = r#"data: {"type":"content_block_start","content_block":{"type":"text","text":"x"}}"#;
        assert!(inspect_sse_line(text_block).content);

        let empty_block = r#"data: {"type":"content_block_start","content_block":{"type":"text","text":""}}"#;
        assert!(!inspect_sse_line(empty_block).content);

        let refusal = r#"data: {"type":"message_delta","delta":{"stop_reason":"refusal"}}"#;
        let info = inspect_sse_line(refusal);
        assert!(!info.content);
        assert_eq!(info.finish_reason.as_deref(), Some(openai_finish::CONTENT_FILTER));
        // A refusal is a legitimate empty response: do not retry it.
        assert!(!should_fail_over(false, info.finish_reason.as_deref()));
    }

    #[test]
    fn responses_events() {
        let delta = r#"data: {"type":"response.output_text.delta","delta":"hi"}"#;
        assert!(inspect_sse_line(delta).content);

        let done = r#"data: {"type":"response.output_text.done","text":"hi"}"#;
        assert!(inspect_sse_line(done).content);

        let call = r#"data: {"type":"response.output_item.added","item":{"type":"function_call"}}"#;
        assert!(inspect_sse_line(call).content);

        let message = r#"data: {"type":"response.output_item.added","item":{"type":"message"}}"#;
        assert!(!inspect_sse_line(message).content);

        let completed = r#"data: {"type":"response.completed"}"#;
        assert_eq!(inspect_sse_line(completed).finish_reason.as_deref(), Some(openai_finish::STOP));

        let incomplete = r#"data: {"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#;
        assert_eq!(inspect_sse_line(incomplete).finish_reason.as_deref(), Some(openai_finish::LENGTH));

        let filtered = r#"data: {"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"}}}"#;
        assert_eq!(inspect_sse_line(filtered).finish_reason.as_deref(), Some(openai_finish::CONTENT_FILTER));

        let other = r#"data: {"type":"response.incomplete","response":{}}"#;
        assert_eq!(inspect_sse_line(other).finish_reason.as_deref(), Some(responses_statuses::INCOMPLETE));

        let failed = r#"data: {"type":"response.failed"}"#;
        assert_eq!(inspect_sse_line(failed).finish_reason.as_deref(), Some(responses_statuses::FAILED));
    }

    #[test]
    fn non_data_lines_carry_nothing() {
        assert_eq!(inspect_sse_line("event: message_delta"), StreamInspection::default());
        assert_eq!(inspect_sse_line("data: [DONE]"), StreamInspection::default());
        assert_eq!(inspect_sse_line(""), StreamInspection::default());
        assert_eq!(inspect_sse_line("data: not json"), StreamInspection::default());
    }

    #[test]
    fn scan_keeps_trailing_fragment_pending() {
        let mut state = ScanState::new();
        scan_sse_buffer("data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\ndata: {\"choi", &mut state);
        assert!(state.saw_content);
        assert_eq!(state.pending, "data: {\"choi");

        // The fragment completes on the next chunk.
        let mut next = ScanState { pending: state.pending, ..ScanState::new() };
        next.saw_content = state.saw_content;
        scan_sse_buffer(&format!("{}ces\":[{{\"delta\":{{}}}}]}}\n", next.pending), &mut next);
        assert!(next.saw_content);
        assert_eq!(next.pending, "");
    }

    #[test]
    fn scan_latches_content_across_chunks() {
        let mut state = ScanState::new();
        scan_sse_buffer("data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n", &mut state);
        assert!(state.saw_content);
        assert_eq!(state.finish_reason.as_deref(), Some("stop"));
        assert!(!should_fail_over(state.saw_content, state.finish_reason.as_deref()));
    }

    #[test]
    fn classify_complete_body() {
        let body = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let (outcome, reason) = classify_body(body);
        assert_eq!(outcome, PeekOutcome::Empty);
        assert_eq!(reason.as_deref(), Some("stop"));
        assert!(should_fail_over(false, reason.as_deref()));

        let content = "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\ndata: [DONE]\n\n";
        assert_eq!(classify_body(content).0, PeekOutcome::Content);
    }

    #[test]
    fn content_filter_is_never_retried() {
        assert!(is_no_retry_finish(Some("content_filter")));
        // Hub vocabulary: case-insensitive and trimmed.
        assert!(is_no_retry_finish(Some(" Content_Filter ")));
        assert!(!is_no_retry_finish(Some("stop")));
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
}
