//! RTK token savers. Port of open-sse/rtk/index.js (compressMessages) and
//! open-sse/rtk/{caveman,ponytail,customPrompt,systemInject}.js.
//!
//! `compress_messages` shrinks tool_result / function output blobs in-place
//! before the request is translated (chatCore's "pre-translate RTK" for
//! cursor, post-translate for everyone else).
//!
//! ponytail / caveman / custom-prompt inject a terse instruction into the
//! system message so the model emits fewer tokens.

use nr_core::Json;

pub const RAW_CAP: usize = 10 * 1024 * 1024; // 10 MiB
pub const MIN_COMPRESS_SIZE: usize = 500; // skip tiny blobs

/// Cumulative stats for one `compress_messages` run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RtkStats {
    pub bytes_before: usize,
    pub bytes_after: usize,
    pub hits: usize,
}

/// Compress tool_result content in-place. Returns `None` when disabled or when
/// the body has no recognizable tool-output shape. Port of `compressMessages`.
///
/// ponytail / caveman text substitution hooks into [`compress_text`]; the
/// filter registry itself is a follow-up (see report).
pub fn compress_messages(body: &mut Json, enabled: bool) -> Option<RtkStats> {
    if !enabled {
        return None;
    }

    // Kiro format: conversationState.history + conversationState.currentMessage
    if body.get("conversationState").is_some() {
        return compress_kiro_format(body);
    }

    // Pick the array field first: a closure in `or_else` would hold a second
    // mutable borrow of `body` while the first is still live.
    let items = if body.get("messages").is_some_and(Json::is_array) {
        body.get_mut("messages")
    } else {
        body.get_mut("input").filter(|v| v.is_array())
    }?;

    let arr = items.as_array_mut()?;
    let mut stats = RtkStats::default();
    for msg in arr.iter_mut() {
        // OpenAI tool message with string content.
        if msg.get("role").and_then(Json::as_str) == Some("tool") {
            if let Some(c) = msg.get_mut("content") {
                if let Some(s) = c.as_str() {
                    *c = Json::from(compress_text(s, &mut stats));
                    continue;
                }
            }
        }
        // Claude tool_result blocks (string or array form).
        let Some(content) = msg.get_mut("content") else { continue };
        if !content.is_array() {
            continue;
        }
        let blocks = content.as_array_mut()?;
        for block in blocks.iter_mut() {
            if block.get("type").and_then(Json::as_str) != Some("tool_result") {
                continue;
            }
            if block.get("is_error").and_then(Json::as_bool) == Some(true) {
                continue; // preserve error traces
            }
            match block.get_mut("content") {
                Some(Json::String(s)) => {
                    let out = compress_text(s, &mut stats);
                    *block.get_mut("content").unwrap() = Json::from(out);
                }
                Some(Json::Array(parts)) => {
                    for part in parts.iter_mut() {
                        if part.get("type").and_then(Json::as_str) == Some("text") {
                            if let Some(Json::String(s)) = part.get_mut("text") {
                                let out = compress_text(s, &mut stats);
                                *part.get_mut("text").unwrap() = Json::from(out);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Some(stats)
}

/// Kiro: conversationState.history[].userInputMessage.userInputMessageContext
/// .toolResults[].content[].text
fn compress_kiro_format(body: &mut Json) -> Option<RtkStats> {
    let mut stats = RtkStats::default();
    let state = body.get_mut("conversationState")?.as_object_mut()?;

    if let Some(Json::Array(hist)) = state.get_mut("history") {
        for item in hist.iter_mut() {
            if let Some(u) = item.get_mut("userInputMessage") {
                compress_kiro_message(u, &mut stats);
            }
        }
    }
    if let Some(u) = state.get_mut("currentMessage").and_then(|m| m.get_mut("userInputMessage")) {
        compress_kiro_message(u, &mut stats);
    }
    Some(stats)
}

/// Compress the tool results of one kiro `userInputMessage`.
fn compress_kiro_message(msg: &mut Json, stats: &mut RtkStats) {
    let Some(tool_results) = msg
        .get_mut("userInputMessageContext")
        .and_then(|c| c.get_mut("toolResults"))
        .and_then(Json::as_array_mut)
    else {
        return;
    };
    for tr in tool_results.iter_mut() {
        if tr.get("status").and_then(Json::as_str) == Some("error") {
            continue;
        }
        let Some(Json::Array(parts)) = tr.get_mut("content") else { continue };
        for part in parts.iter_mut() {
            if let Some(Json::String(s)) = part.get_mut("text") {
                let out = compress_text(s, stats);
                *part.get_mut("text").unwrap() = Json::from(out);
            }
        }
    }
}

/// Compress one text blob. Placeholder pass-through with the same size guards
/// as the JS `compressText` — safety: never return empty, never grow.
///
/// ponytail / caveman: add the filter registry here, keeping the guard above.
fn compress_text(text: &str, stats: &mut RtkStats) -> String {
    let bytes_in = text.len();
    stats.bytes_before += bytes_in;

    if bytes_in < MIN_COMPRESS_SIZE || bytes_in > RAW_CAP {
        stats.bytes_after += bytes_in;
        return text.to_string();
    }

    // TODO(dedupe): autoDetectFilter + safeApply (open-sse/rtk/autodetect.js) —
    // needs the native rtk filter tables. Until then, passthrough never grows.
    stats.bytes_after += bytes_in;
    text.to_string()
}

/// Inject a terse-style system prompt (caveman / ponytail / custom).
/// Port of open-sse/rtk/systemInject.js `injectSystemPrompt`.
///
/// Returns true when the prompt was appended (JS `injectCustomPrompt`).
pub fn inject_system_prompt(body: &mut Json, prompt: &str) -> bool {
    if prompt.trim().is_empty() {
        return false;
    }
    if let Some(obj) = body.as_object_mut() {
        match obj.get_mut("system") {
            Some(Json::String(s)) => {
                if !s.contains(prompt) {
                    if !s.is_empty() {
                        s.push_str("\n\n");
                    }
                    s.push_str(prompt);
                }
            }
            Some(Json::Array(arr)) => {
                let already = arr.iter().any(|p| p.get("text").and_then(Json::as_str) == Some(prompt));
                if !already {
                    arr.push(Json::from(serde_json::json!({"type": "text", "text": prompt})));
                }
            }
            _ => {
                obj.insert("system".into(), Json::from(prompt));
            }
        }
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn disabled_returns_none() {
        let mut body = json!({"messages": [{"role": "tool", "content": "x"}]});
        assert_eq!(compress_messages(&mut body, false), None);
    }

    #[test]
    fn openai_tool_string_is_counted() {
        let blob = "x".repeat(600);
        let mut body = json!({"messages": [{"role": "tool", "content": blob}]});
        let stats = compress_messages(&mut body, true).unwrap();
        assert_eq!(stats.bytes_before, 600);
        // Passthrough compressor: bytes_after == bytes_before
        assert_eq!(stats.bytes_after, 600);
    }

    #[test]
    fn error_traces_are_preserved() {
        let mut body = json!({
            "messages": [{
                "role": "user",
                "content": [{"type": "tool_result", "is_error": true, "content": "raw"}]
            }]
        });
        let stats = compress_messages(&mut body, true).unwrap();
        assert_eq!(stats.bytes_before, 0, "is_error blob skipped");
        assert_eq!(
            body["messages"][0]["content"][0]["content"],
            json!("raw"),
            "error trace untouched"
        );
    }

    #[test]
    fn tiny_blob_below_min_is_skipped() {
        let mut body = json!({"messages": [{"role": "tool", "content": "short"}]});
        let stats = compress_messages(&mut body, true).unwrap();
        assert_eq!(stats.bytes_before, 5);
        assert_eq!(stats.bytes_after, 5);
    }

    #[test]
    fn kiro_tool_results_are_compressed() {
        let mut body = json!({
            "conversationState": {
                "history": [{
                    "userInputMessage": {
                        "userInputMessageContext": {
                            "toolResults": [{ "status": "ok", "content": [{ "text": "abc" }] }]
                        }
                    }
                }]
            }
        });
        let stats = compress_messages(&mut body, true);
        assert!(stats.is_some());
        assert_eq!(stats.unwrap().bytes_before, 3);
    }

    #[test]
    fn no_messages_shape_returns_none() {
        let mut body = json!({"model": "gpt-4o"});
        assert_eq!(compress_messages(&mut body, true), None);
    }

    #[test]
    fn system_prompt_string_appended_idempotently() {
        let mut body = json!({"system": "You are helpful."});
        assert!(inject_system_prompt(&mut body, "Be terse."));
        let s = body["system"].as_str().unwrap();
        assert_eq!(s, "You are helpful.\n\nBe terse.");

        // Second injection is a no-op.
        inject_system_prompt(&mut body, "Be terse.");
        assert_eq!(body["system"].as_str().unwrap(), "You are helpful.\n\nBe terse.");
    }

    #[test]
    fn system_prompt_array_block_appended() {
        let mut body = json!({"system": [{"type": "text", "text": "base"}]});
        assert!(inject_system_prompt(&mut body, "Be terse."));
        let arr = body["system"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[1]["text"], json!("Be terse."));
    }

    #[test]
    fn empty_prompt_is_ignored() {
        let mut body = json!({});
        assert!(!inject_system_prompt(&mut body, "   "));
    }
}
