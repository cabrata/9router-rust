//! Tool-call emulation helpers for web-cookie executors — port of
//! open-sse/executors/chatgptWebTools.js.
//!
//! Web-cookie providers (Perplexity Web, Gemini Web, etc.) may have no native
//! function calling. When the OpenAI request carries `tools`, the prompt-side
//! shim (`prepare_tool_messages`) injects a `<tool>` contract; on the response
//! side we parse `<tool>{...}</tool>` blocks back into OpenAI `tool_calls`.
//!
//! The whole tool-mode orchestration lives here — provider-agnostic — so each
//! executor only gains an import and a single delegating call. Despite the
//! filename (kept from the first caller), this module is shared:
//! `build_tool_mode_response()` accepts an `id_seed` so every provider gets its
//! own `tool_calls[].id` prefix.

use bytes::Bytes;
use http::header::CONTENT_TYPE;
use http::HeaderMap;
use nr_common::sse::sse_chunk;
use nr_core::http::UpstreamResponse;
use nr_core::Json;
use serde_json::json;

use crate::web_tools::{build_tool_aware_result, ToolCall};

/// Replay metadata for the streaming path (`meta` in the JS signature).
#[derive(Clone, Debug)]
pub struct ToolModeMeta {
    pub cid: String,
    pub created: i64,
    pub model: String,
    /// Prefix for generated `tool_calls[].id`s; `"cgpt"` by default in JS.
    pub id_seed: String,
}

impl ToolModeMeta {
    pub fn new(cid: impl Into<String>, created: i64, model: impl Into<String>) -> Self {
        Self {
            cid: cid.into(),
            created,
            model: model.into(),
            id_seed: "cgpt".to_string(),
        }
    }

    pub fn with_id_seed(mut self, id_seed: impl Into<String>) -> Self {
        self.id_seed = id_seed.into();
        self
    }
}

fn json_response(status: u16, body: String) -> UpstreamResponse {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, http::HeaderValue::from_static("application/json"));
    UpstreamResponse::from_bytes(status, headers, body)
}

pub(crate) fn sse_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        CONTENT_TYPE,
        http::HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-cache"),
    );
    headers.insert(
        http::HeaderName::from_static("x-accel-buffering"),
        http::HeaderValue::from_static("no"),
    );
    headers
}

fn tool_call_to_json(call: &ToolCall) -> Json {
    json!({
        "id": call.id,
        "type": "function",
        "function": {
            "name": call.function.name,
            "arguments": call.function.arguments,
        }
    })
}

/// `applyToolCallsToJsonResponse` — parse `<tool>` blocks in a buffered JSON
/// completion into `tool_calls` and rewrite the choice. On any structural
/// surprise the original body passes through untouched (JS relies on the
/// surrounding try/catch for the missing-`choices` case).
fn apply_tool_calls_to_json_response(
    body_text: &str,
    requested_tools: &[Json],
    id_seed: &str,
) -> String {
    let Ok(mut parsed) = serde_json::from_str::<Json>(body_text) else {
        return body_text.to_string();
    };

    let has_choice = parsed
        .get("choices")
        .and_then(Json::as_array)
        .and_then(|c| c.first())
        .is_some_and(Json::is_object);
    if !has_choice {
        // `json.choices[0].message` throws in JS → caught → original body.
        return body_text.to_string();
    }

    let raw_content = parsed["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .to_string();

    let (content, tool_calls, finish_reason) =
        build_tool_aware_result(&raw_content, requested_tools, id_seed);

    match tool_calls {
        Some(calls) if !calls.is_empty() => {
            let tc_json: Vec<Json> = calls.iter().map(tool_call_to_json).collect();
            parsed["choices"][0]["message"] = json!({
                "role": "assistant",
                "content": Json::Null,
                "tool_calls": tc_json,
            });
            parsed["choices"][0]["finish_reason"] = Json::String(finish_reason.to_string());
        }
        _ => {
            parsed["choices"][0]["message"]["content"] = Json::String(content);
        }
    }

    parsed.to_string()
}

/// `toolCompletionToSseStream` — replay an already-built OpenAI
/// `chat.completion` object as a buffered SSE stream: a role chunk, one delta
/// per tool call, then a terminal chunk carrying `finish_reason`. Every
/// `delta.tool_calls` entry carries `index`; clients accumulate parallel calls
/// by that field, and a missing index collapses them into the first slot.
/// No token-by-token streaming while tools are active.
fn tool_completion_to_sse(completion: &Json, cid: &str, created: i64, model: &str) -> String {
    let choice = completion.get("choices").and_then(|c| c.get(0));
    let message = choice.and_then(|c| c.get("message")).cloned().unwrap_or(Json::Null);
    let finish_reason = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(Json::as_str)
        .unwrap_or("stop")
        .to_string();

    let chunk = |delta: Json, finish: Json| -> String {
        sse_chunk(&json!({
            "id": cid,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "system_fingerprint": Json::Null,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish, "logprobs": Json::Null }],
        }))
    };

    let mut out = String::new();
    out.push_str(&chunk(json!({ "role": "assistant" }), Json::Null));

    let calls = message.get("tool_calls").and_then(Json::as_array);
    match calls.filter(|c| !c.is_empty()) {
        Some(calls) => {
            for (index, call) in calls.iter().enumerate() {
                let mut entry = serde_json::Map::new();
                entry.insert("index".to_string(), json!(index));
                if let Some(obj) = call.as_object() {
                    for (key, value) in obj {
                        entry.insert(key.clone(), value.clone());
                    }
                }
                out.push_str(&chunk(
                    json!({ "tool_calls": [Json::Object(entry)] }),
                    Json::Null,
                ));
            }
            out.push_str(&chunk(json!({}), Json::String(finish_reason)));
        }
        None => {
            let content = message
                .get("content")
                .filter(|c| !c.is_null())
                .cloned()
                .unwrap_or_else(|| Json::String(String::new()));
            out.push_str(&chunk(json!({ "content": content }), Json::String(finish_reason)));
        }
    }
    out.push_str("data: [DONE]\n\n");
    out
}

/// `buildToolModeResponse(bufferedJson, requestedTools, stream, meta)`.
///
/// Tool mode: parse `<tool>` blocks in an already-buffered JSON completion into
/// `tool_calls`, then return either the JSON completion (non-streaming) or a
/// terminal SSE replay of it (streaming).
pub fn build_tool_mode_response(
    status: u16,
    body_text: &str,
    requested_tools: &[Json],
    stream: bool,
    meta: &ToolModeMeta,
) -> UpstreamResponse {
    if !(200..300).contains(&status) {
        // `if (!bufferedJson.ok) return bufferedJson;`
        return UpstreamResponse::from_bytes(status, HeaderMap::new(), body_text.to_string());
    }

    let rewritten = apply_tool_calls_to_json_response(body_text, requested_tools, &meta.id_seed);
    if !stream {
        return json_response(status, rewritten);
    }

    let completion: Json = match serde_json::from_str(&rewritten) {
        Ok(v) => v,
        // Rewriting only ever produces valid JSON, but never panic on input.
        Err(_) => return json_response(status, rewritten),
    };
    let body = tool_completion_to_sse(&completion, &meta.cid, meta.created, &meta.model);
    let bytes: Bytes = Bytes::from(body);
    let body_stream = Box::pin(futures::stream::once(async move {
        Ok::<Bytes, std::io::Error>(bytes)
    }));
    UpstreamResponse::from_stream(200, sse_headers(), body_stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> Vec<Json> {
        vec![json!({"type": "function", "function": {
            "name": "weather",
            "description": "weather lookup",
            "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
        }})]
    }

    fn completion(content: &str) -> String {
        json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "created": 1,
            "model": "gpt-x",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": content },
                "finish_reason": "stop"
            }]
        })
        .to_string()
    }

    fn meta() -> ToolModeMeta {
        ToolModeMeta::new("cid-1", 42, "gpt-x")
    }

    #[test]
    fn non_ok_status_passes_through() {
        let body = r#"{"error":{"message":"nope"}}"#;
        let resp = build_tool_mode_response(500, body, &tools(), false, &meta());
        assert_eq!(resp.status, 500);
    }

    #[tokio::test]
    async fn json_mode_extracts_tool_calls() {
        let body = completion("calling <tool>{\"name\": \"weather\", \"arguments\": {\"city\": \"Paris\"}}</tool> now");
        let resp = build_tool_mode_response(200, &body, &tools(), false, &meta());
        let text = resp.text().await.unwrap();
        let parsed: Json = serde_json::from_str(&text).unwrap();
        let choice = &parsed["choices"][0];
        assert_eq!(choice["finish_reason"], "tool_calls");
        assert_eq!(choice["message"]["role"], "assistant");
        assert!(choice["message"]["content"].is_null());
        assert_eq!(choice["message"]["tool_calls"][0]["function"]["name"], "weather");
        assert_eq!(choice["message"]["tool_calls"][0]["type"], "function");
        assert_eq!(
            choice["message"]["tool_calls"][0]["id"],
            "cgpt_0",
            "id seed prefixes generated ids"
        );
        // Original envelope keys survive the rewrite.
        assert_eq!(parsed["id"], "chatcmpl-1");
        assert_eq!(parsed["model"], "gpt-x");
    }

    #[tokio::test]
    async fn json_mode_keeps_content_when_no_tool_calls() {
        let body = completion("just prose");
        let resp = build_tool_mode_response(200, &body, &tools(), false, &meta());
        let parsed: Json = serde_json::from_str(&resp.text().await.unwrap()).unwrap();
        assert_eq!(parsed["choices"][0]["message"]["content"], "just prose");
        assert_eq!(parsed["choices"][0]["finish_reason"], "stop");
        assert!(parsed["choices"][0]["message"].get("tool_calls").is_none());
    }

    #[tokio::test]
    async fn invalid_json_passes_through_untouched() {
        let body = "not json at all";
        let resp = build_tool_mode_response(200, body, &tools(), false, &meta());
        assert_eq!(resp.text().await.unwrap(), body);
    }

    #[tokio::test]
    async fn missing_choices_passes_through_untouched() {
        let body = r#"{"id":"x","object":"chat.completion"}"#;
        let resp = build_tool_mode_response(200, body, &tools(), false, &meta());
        assert_eq!(resp.text().await.unwrap(), body);
    }

    #[tokio::test]
    async fn empty_content_does_not_break_rewrite() {
        let body = json!({
            "choices": [{ "index": 0, "message": { "role": "assistant", "content": Json::Null } }]
        })
        .to_string();
        let resp = build_tool_mode_response(200, &body, &tools(), false, &meta());
        let parsed: Json = serde_json::from_str(&resp.text().await.unwrap()).unwrap();
        assert_eq!(parsed["choices"][0]["message"]["content"], "");
    }

    #[tokio::test]
    async fn custom_id_seed_is_used() {
        let body = completion("<tool>{\"name\": \"weather\", \"arguments\": {\"city\": \"Oslo\"}}</tool>");
        let resp = build_tool_mode_response(
            200,
            &body,
            &tools(),
            false,
            &meta().with_id_seed("gwe"),
        );
        let parsed: Json = serde_json::from_str(&resp.text().await.unwrap()).unwrap();
        assert_eq!(parsed["choices"][0]["message"]["tool_calls"][0]["id"], "gwe_0");
    }

    #[tokio::test]
    async fn stream_mode_replays_tool_calls_then_done() {
        let body = completion("<tool>{\"name\": \"weather\", \"arguments\": {\"city\": \"Paris\"}}</tool>");
        let resp = build_tool_mode_response(200, &body, &tools(), true, &meta());
        assert_eq!(resp.status, 200);
        assert_eq!(resp.content_type(), "text/event-stream");
        let text = resp.text().await.unwrap();

        let frames: Vec<&str> = text
            .split("\n\n")
            .filter(|s| !s.trim().is_empty())
            .collect();
        assert_eq!(frames[0], r#"data: {"id":"cid-1","object":"chat.completion.chunk","created":42,"model":"gpt-x","system_fingerprint":null,"choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null,"logprobs":null}]}"#);
        assert!(frames[1].contains(r#""tool_calls":[{"index":0,"id":"cgpt_0""#));
        assert!(frames[2].contains(r#""finish_reason":"tool_calls""#));
        assert_eq!(frames[3], "data: [DONE]");
        assert_eq!(frames.len(), 4);
    }

    #[tokio::test]
    async fn stream_mode_emits_content_chunk_without_tools() {
        let body = completion("hello");
        let resp = build_tool_mode_response(200, &body, &tools(), true, &meta());
        let text = resp.text().await.unwrap();
        let frames: Vec<&str> = text
            .split("\n\n")
            .filter(|s| !s.trim().is_empty())
            .collect();
        assert_eq!(frames.len(), 3);
        assert!(frames[1].contains(r#""delta":{"content":"hello"},"finish_reason":"stop""#));
        assert_eq!(frames[2], "data: [DONE]");
    }

    #[tokio::test]
    async fn parallel_tool_calls_get_ascending_indices() {
        let tools = vec![
            json!({"type": "function", "function": {"name": "a", "parameters": {"type": "object"}}}),
            json!({"type": "function", "function": {"name": "b", "parameters": {"type": "object"}}}),
        ];
        let body = completion("<tool>{\"name\": \"a\", \"arguments\": {}}</tool><tool>{\"name\": \"b\", \"arguments\": {}}</tool>");
        let resp = build_tool_mode_response(200, &body, &tools, true, &meta());
        let text = resp.text().await.unwrap();
        assert!(text.contains(r#""tool_calls":[{"index":0,"id":"cgpt_0""#));
        assert!(text.contains(r#""tool_calls":[{"index":1,"id":"cgpt_1""#));
    }
}
