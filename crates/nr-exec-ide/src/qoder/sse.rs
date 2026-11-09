//! Port of open-sse/shared/qoder/sse.js — the `{statusCodeValue, body}`
//! envelope unwrapper / chunk coalescer.
//!
//! Qoder SSE is OpenAI-shaped inside envelopes, but usage arrives on a later
//! `choices: []` frame — after finish_reason, which itself often lives on
//! `delta.finish_reason` rather than the choice. Downstream clients read usage
//! only from the finish chunk, so hold empty finish and usage-only frames and
//! emit one OpenAI include_usage-style chunk with both.

use nr_common::sse_constants::SSE_DONE;
use nr_core::Json;
use serde_json::json;

fn num_of(v: Option<&Json>) -> Option<f64> {
    match v? {
        Json::Number(n) => n.as_f64().filter(|f| f.is_finite()),
        Json::String(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()),
        _ => None,
    }
}

fn json_num(v: f64) -> Json {
    if v.fract() == 0.0 && v.abs() < 9.007_199_254_740_992e15 {
        json!(v as i64)
    } else {
        json!(v)
    }
}

/// `canonicalizeQoderUsage` — normalize Qoder/OpenAI usage into the shape
/// stream.js + Claude translation understand.
pub fn canonicalize_qoder_usage(usage: &Json) -> Option<Json> {
    let map = usage.as_object()?;
    let prompt = num_of(map.get("prompt_tokens").or_else(|| map.get("input_tokens")));
    let completion = num_of(map.get("completion_tokens").or_else(|| map.get("output_tokens")));
    if prompt.is_none() && completion.is_none() {
        return None;
    }

    let mut details = map
        .get("prompt_tokens_details")
        .and_then(Json::as_object)
        .cloned()
        .unwrap_or_default();

    let cached = num_of(
        details
            .get("cached_tokens")
            .or_else(|| map.get("cached_tokens"))
            .or_else(|| map.get("prompt_cache_hit_tokens"))
            .or_else(|| map.get("cache_read_input_tokens")),
    );
    let cache_creation = num_of(
        details
            .get("cache_creation_tokens")
            .or_else(|| map.get("cache_creation_input_tokens")),
    );

    let prompt_tokens = prompt.unwrap_or(0.0);
    let completion_tokens = completion.unwrap_or(0.0);
    let mut out = json!({
        "prompt_tokens": json_num(prompt_tokens),
        "completion_tokens": json_num(completion_tokens),
    });
    out["total_tokens"] = map
        .get("total_tokens")
        .and_then(|v| num_of(Some(v)))
        .map(json_num)
        .unwrap_or_else(|| json_num(prompt_tokens + completion_tokens));

    if let Some(cached) = cached {
        out["cached_tokens"] = json_num(cached);
        details.insert("cached_tokens".into(), json_num(cached));
    }
    if let Some(creation) = cache_creation {
        details.insert("cache_creation_tokens".into(), json_num(creation));
    }
    if !details.is_empty() {
        out["prompt_tokens_details"] = Json::Object(details);
    }
    if let Some(ctd) = map.get("completion_tokens_details").filter(|v| v.is_object()) {
        out["completion_tokens_details"] = ctd.clone();
    }
    let reasoning = num_of(
        map.get("reasoning_tokens").or_else(|| {
            map.get("completion_tokens_details")
                .and_then(Json::as_object)
                .and_then(|d| d.get("reasoning_tokens"))
        }),
    );
    if let Some(r) = reasoning {
        out["reasoning_tokens"] = json_num(r);
    }
    Some(out)
}

fn finish_reason_of(parsed: &Json) -> Option<String> {
    let choice = parsed.get("choices").and_then(Json::as_array).and_then(|a| a.first());
    choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(Json::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            choice
                .and_then(|c| c.get("delta"))
                .and_then(|d| d.get("finish_reason"))
                .and_then(Json::as_str)
                .filter(|s| !s.is_empty())
        })
        .or_else(|| parsed.get("finish_reason").and_then(Json::as_str).filter(|s| !s.is_empty()))
        .map(String::from)
}

fn has_valuable_delta(parsed: &Json) -> bool {
    let Some(delta) = parsed
        .get("choices")
        .and_then(Json::as_array)
        .and_then(|a| a.first())
        .and_then(|c| c.get("delta"))
    else {
        return false;
    };
    if !delta.is_object() {
        return false;
    }
    if delta.get("content").and_then(Json::as_str).map(|s| !s.is_empty()).unwrap_or(false) {
        return true;
    }
    if delta.get("reasoning_content").and_then(Json::as_str).map(|s| !s.is_empty()).unwrap_or(false) {
        return true;
    }
    if delta.get("tool_calls").and_then(Json::as_array).map(|a| !a.is_empty()).unwrap_or(false) {
        return true;
    }
    delta.get("role").map(|v| nr_core::json::truthy(v)).unwrap_or(false)
}

enum ParsedInner {
    Done,
    Parsed(Json),
    Raw(String),
    Empty,
}

fn parse_inner(inner: &str) -> ParsedInner {
    if inner.is_empty() {
        return ParsedInner::Empty;
    }
    if inner == "[DONE]" {
        return ParsedInner::Done;
    }
    match serde_json::from_str::<Json>(inner) {
        Ok(v) => ParsedInner::Parsed(v),
        Err(_) => ParsedInner::Raw(inner.to_string()),
    }
}

/// Byte sink for the coalescer. Streaming callers push into a channel; tests
/// collect into a Vec.
pub trait ChunkSink {
    fn push(&mut self, bytes: &str);
}

impl ChunkSink for String {
    fn push(&mut self, bytes: &str) {
        self.push_str(bytes);
    }
}

impl ChunkSink for Vec<String> {
    fn push(&mut self, bytes: &str) {
        self.push(bytes.to_string());
    }
}

/// `createQoderSseCoalescer({ model, encoder, sseDone })` — stateful unwrapper.
pub struct QoderSseCoalescer {
    model: String,
    pub done_emitted: bool,
    pending_finish: Option<String>,
    pending_usage: Option<Json>,
    last_id: Option<String>,
    last_created: Option<i64>,
    last_model: Option<String>,
    finish_already_forwarded: bool,
}

impl QoderSseCoalescer {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            done_emitted: false,
            pending_finish: None,
            pending_usage: None,
            last_id: None,
            last_created: None,
            last_model: None,
            finish_already_forwarded: false,
        }
    }

    fn now_ms() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    fn now_secs() -> i64 {
        chrono::Utc::now().timestamp()
    }

    fn emit_json<S: ChunkSink>(sink: &mut S, obj: &Json) {
        // Every emitted frame is single-line: a literal newline inside a JSON
        // string would break the SSE framing downstream.
        let sanitized = obj.to_string().replace('\r', "").replace('\n', "");
        sink.push(&format!("data: {sanitized}\n\n"));
    }

    fn emit_raw<S: ChunkSink>(sink: &mut S, text: &str) {
        sink.push(&format!("data: {}\n\n", text.replace('\r', "").replace('\n', "")));
    }

    fn emit_done<S: ChunkSink>(&mut self, sink: &mut S) {
        if self.done_emitted {
            return;
        }
        sink.push(SSE_DONE);
        self.done_emitted = true;
    }

    fn emit_terminal<S: ChunkSink>(&mut self, sink: &mut S) {
        if self.pending_finish.is_none() && self.pending_usage.is_none() {
            return;
        }
        let id = self
            .last_id
            .clone()
            .unwrap_or_else(|| format!("qoder-{}", Self::now_ms()));
        let created = self.last_created.unwrap_or_else(Self::now_secs);
        let model = self.last_model.clone().unwrap_or_else(|| self.model.clone());
        let mut chunk = json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": self.pending_finish.clone().unwrap_or_else(|| "stop".into()),
            }],
        });
        if let Some(usage) = self.pending_usage.take() {
            chunk["usage"] = usage;
        }
        Self::emit_json(sink, &chunk);
        self.pending_finish = None;
    }

    /// Flush any held finish/usage frame, then `[DONE]`.
    pub fn flush<S: ChunkSink>(&mut self, sink: &mut S) {
        if self.done_emitted {
            return;
        }
        if self.pending_usage.is_some() || (self.pending_finish.is_some() && !self.finish_already_forwarded) {
            self.emit_terminal(sink);
        }
        self.emit_done(sink);
    }

    /// Handle one unwrapped envelope body. Returns `true` once the stream is
    /// terminal (the caller should stop reading upstream).
    pub fn handle_inner<S: ChunkSink>(&mut self, inner: &str, sink: &mut S) -> bool {
        if self.done_emitted {
            return true;
        }

        match parse_inner(inner) {
            ParsedInner::Empty => false,
            ParsedInner::Done => {
                self.flush(sink);
                true
            }
            ParsedInner::Raw(text) => {
                Self::emit_raw(sink, &text);
                false
            }
            ParsedInner::Parsed(parsed) => self.handle_parsed(&parsed, inner, sink),
        }
    }

    fn handle_parsed<S: ChunkSink>(&mut self, parsed: &Json, inner: &str, sink: &mut S) -> bool {
        if !parsed.is_object() {
            return false;
        }

        if let Some(id) = parsed.get("id").and_then(Json::as_str).filter(|s| !s.is_empty()) {
            self.last_id = Some(id.to_string());
        }
        if let Some(created) = parsed.get("created").and_then(Json::as_i64) {
            self.last_created = Some(created);
        }
        if let Some(model) = parsed.get("model").and_then(Json::as_str).filter(|s| !s.is_empty()) {
            self.last_model = Some(model.to_string());
        }

        if let Some(usage) = parsed.get("usage").and_then(canonicalize_qoder_usage) {
            self.pending_usage = Some(usage);
        }

        let finish = finish_reason_of(parsed);
        if has_valuable_delta(parsed) {
            // Stream content as-is (preserves upstream JSON for tests/clients).
            Self::emit_raw(sink, inner);
            if let Some(f) = finish {
                self.finish_already_forwarded = true;
                // Keep finish around only if we still need a usage trailer.
                self.pending_finish = if self.pending_usage.is_some() { Some(f) } else { None };
            }
            if self.pending_finish.is_some() && self.pending_usage.is_some() {
                self.emit_terminal(sink);
                self.emit_done(sink);
                return true;
            }
            return false;
        }

        if finish.is_some() {
            self.pending_finish = finish;
        }

        // Empty finish and/or usage-only: emit as soon as we have both (Qoder
        // order is finish then usage). Don't wait for the later [DONE].
        if (self.pending_finish.is_some() || self.finish_already_forwarded) && self.pending_usage.is_some() {
            if self.pending_finish.is_none() {
                self.pending_finish = Some("stop".into());
            }
            self.emit_terminal(sink);
            self.emit_done(sink);
            return true;
        }
        false
    }
}

/// Unwrap one Qoder envelope line (`data: {"statusCodeValue":200,"body":"..."}`).
///
/// Returns `Ok(inner)` for a success frame, `Err((status, message))` for a
/// non-200 statusCodeValue, `Ok(None)` for `[DONE]` or a non-data line.
pub fn unwrap_envelope_line(line: &str) -> EnvelopeLine {
    let trimmed = line.trim_end_matches('\r').trim();
    if trimmed.is_empty() || !trimmed.starts_with("data:") {
        return EnvelopeLine::Skip;
    }
    let data = trimmed[5..].trim_start();
    if data == "[DONE]" {
        return EnvelopeLine::Done;
    }
    let Ok(envelope) = serde_json::from_str::<Json>(data) else {
        return EnvelopeLine::Skip;
    };
    let status_val = envelope
        .get("statusCodeValue")
        .and_then(|v| match v {
            Json::Number(n) => n.as_f64(),
            Json::String(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        })
        .filter(|f| f.is_finite())
        .map(|f| f as i64)
        .unwrap_or(200);
    let inner = match envelope.get("body") {
        Some(Json::String(s)) => s.clone(),
        Some(Json::Null) | None => String::new(),
        Some(other) => other.to_string(),
    };
    if status_val != 200 {
        return EnvelopeLine::Error { status: status_val, message: inner };
    }
    EnvelopeLine::Frame(inner)
}

/// One decoded Qoder envelope line.
#[derive(Debug, Clone, PartialEq)]
pub enum EnvelopeLine {
    /// Line is not an SSE data line, or was not JSON.
    Skip,
    Done,
    Frame(String),
    Error { status: i64, message: String },
}

/// `isBillingBlock` — code 110/112 or a `pricingUrl` field.
///
/// Code 10605 is deliberately NOT here: it is the model-queue throttle, a
/// transient capacity signal, and mapping it to a 403 would make chatCore lock
/// the model for minutes instead of waiting ~30s.
pub fn is_billing_block(inner: &str) -> bool {
    if inner.is_empty() {
        return false;
    }
    if inner.to_lowercase().contains("pricingurl") {
        return true;
    }
    if let Ok(parsed) = serde_json::from_str::<Json>(inner) {
        let code = parsed.get("code").map(|c| match c {
            Json::String(s) => s.clone(),
            Json::Number(n) => n.to_string(),
            _ => String::new(),
        });
        if matches!(code.as_deref(), Some("110") | Some("112")) {
            return true;
        }
    }
    // Legacy exact shape: {"code":"112",...}
    inner.contains("\"code\":\"112\"") || inner.contains("\"code\": \"112\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_frames(out: &str) -> Vec<Json> {
        out.lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter(|d| *d != "[DONE]")
            .filter_map(|d| serde_json::from_str::<Json>(d).ok())
            .collect()
    }

    #[test]
    fn unwraps_success_envelope() {
        let line = r#"data: {"statusCodeValue":200,"body":"{\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}"}"#;
        match unwrap_envelope_line(line) {
            EnvelopeLine::Frame(inner) => {
                let parsed: Json = serde_json::from_str(&inner).unwrap();
                assert_eq!(parsed["choices"][0]["delta"]["content"], json!("hi"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn unwraps_object_body_and_numeric_string_status() {
        let line = r#"data: {"statusCodeValue":"500","body":{"message":"boom"}}"#;
        assert_eq!(
            unwrap_envelope_line(line),
            EnvelopeLine::Error { status: 500, message: "{\"message\":\"boom\"}".into() }
        );
    }

    #[test]
    fn skips_non_data_lines_and_handles_done() {
        assert_eq!(unwrap_envelope_line(""), EnvelopeLine::Skip);
        assert_eq!(unwrap_envelope_line("event: ping"), EnvelopeLine::Skip);
        assert_eq!(unwrap_envelope_line("data: [DONE]"), EnvelopeLine::Done);
        assert_eq!(unwrap_envelope_line("data: not json"), EnvelopeLine::Skip);
    }

    #[test]
    fn billing_block_signatures() {
        assert!(is_billing_block(r#"{"code":"110"}"#));
        assert!(is_billing_block(r#"{"code":112,"message":"quota"}"#));
        assert!(is_billing_block(r#"{"pricingUrl":"https://x"}"#));
        assert!(is_billing_block(r#"{"code":"112","x":1}"#));
        // Queue throttle is not a billing block.
        assert!(!is_billing_block(r#"{"code":"10605","subCode":"RESOURCE_LIMIT"}"#));
        assert!(!is_billing_block(""));
    }

    #[test]
    fn canonicalizes_usage_aliases() {
        let usage = json!({
            "input_tokens": 10,
            "output_tokens": 5,
            "prompt_tokens_details": { "cache_creation_tokens": 3 }
        });
        let out = canonicalize_qoder_usage(&usage).unwrap();
        assert_eq!(out["prompt_tokens"], json!(10));
        assert_eq!(out["completion_tokens"], json!(5));
        assert_eq!(out["total_tokens"], json!(15));
        assert_eq!(out["prompt_tokens_details"]["cache_creation_tokens"], json!(3));
        assert!(canonicalize_qoder_usage(&json!({"x": 1})).is_none());
    }

    #[test]
    fn coalesces_finish_and_usage_into_one_chunk() {
        let mut c = QoderSseCoalescer::new("auto");
        let mut out = String::new();

        let content = json!({ "id": "a", "created": 5, "choices": [{ "index": 0, "delta": { "content": "hi" } }] });
        assert!(!c.handle_inner(&content.to_string(), &mut out));

        // finish_reason on `delta`, no content: held back.
        let finish = json!({ "id": "a", "created": 5, "choices": [{ "index": 0, "delta": { "finish_reason": "stop" } }] });
        assert!(!c.handle_inner(&finish.to_string(), &mut out));

        let usage = json!({ "id": "a", "created": 5, "choices": [], "usage": { "prompt_tokens": 7, "completion_tokens": 2 } });
        assert!(c.handle_inner(&usage.to_string(), &mut out), "terminal once both are present");

        let frames = collect_frames(&out);
        assert_eq!(frames.len(), 2, "content frame + coalesced terminal: {out}");
        assert_eq!(frames[0]["choices"][0]["delta"]["content"], json!("hi"));
        assert_eq!(frames[1]["choices"][0]["delta"], json!({}));
        assert_eq!(frames[1]["choices"][0]["finish_reason"], json!("stop"));
        assert_eq!(frames[1]["usage"]["total_tokens"], json!(9));
        assert!(out.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn flushes_content_frame_with_finish_and_usage_together() {
        // Upstream sometimes packs content + finish, then usage separately.
        let mut c = QoderSseCoalescer::new("auto");
        let mut out = String::new();
        let chunk = json!({
            "id": "a", "created": 5,
            "choices": [{ "index": 0, "delta": { "content": "done" }, "finish_reason": "stop" }]
        });
        assert!(!c.handle_inner(&chunk.to_string(), &mut out));
        let usage = json!({ "choices": [], "usage": { "prompt_tokens": 1, "completion_tokens": 1 } });
        assert!(c.handle_inner(&usage.to_string(), &mut out));
        let frames = collect_frames(&out);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1]["choices"][0]["finish_reason"], json!("stop"));
        assert_eq!(frames[1]["usage"]["prompt_tokens"], json!(1));
    }

    #[test]
    fn done_flushes_pending_finish() {
        let mut c = QoderSseCoalescer::new("auto");
        let mut out = String::new();
        let finish = json!({ "choices": [{ "index": 0, "delta": { "finish_reason": "length" } }] });
        assert!(!c.handle_inner(&finish.to_string(), &mut out));
        assert!(c.handle_inner("[DONE]", &mut out));
        let frames = collect_frames(&out);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["choices"][0]["finish_reason"], json!("length"));
        assert!(out.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn raw_inner_text_is_forwarded_unchanged() {
        let mut c = QoderSseCoalescer::new("auto");
        let mut out = String::new();
        assert!(!c.handle_inner("plain text", &mut out));
        assert_eq!(out, "data: plain text\n\n");
    }

    #[test]
    fn nothing_pending_still_emits_done_once() {
        let mut c = QoderSseCoalescer::new("auto");
        let mut out = String::new();
        c.flush(&mut out);
        c.flush(&mut out);
        assert_eq!(out, "data: [DONE]\n\n");
        assert!(c.done_emitted);
    }
}
