//! Port of open-sse/utils/usageTracking.js.
//!
//! Token usage extraction, normalization, estimation and logging. All values are
//! `serde_json::Value` so callers can forward/merge across formats unchanged.

use nr_core::format;
use serde_json::Value;

/// Buffer tokens added to estimated usage to prevent context errors.
const BUFFER_TOKENS: i64 = 2000;

/// Legacy per-chunk usage console line; off by default (superseded by "📊 done").
fn debug_usage_enabled() -> bool {
    std::env::var("LOG_USAGE_VERBOSE").map(|v| v == "1").unwrap_or(false)
}

fn as_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::Null => Some(0.0),
        _ => None,
    }
}

fn num_or_zero(v: Option<&Value>) -> i64 {
    v.and_then(as_number).map(|f| f as i64).unwrap_or(0)
}

fn is_numberish(v: &Value) -> bool {
    as_number(v).is_some_and(|f| f.is_finite())
}

/// `Number(value)` in JS: integral floats come out as integers. serde_json has two
/// representations (PosInt/F64), and `Value::from(40.0)` serializes as `40.0` rather
/// than `40`, so we collapse integral values to i64 to match the JS wire shape.
fn as_json_number(v: f64) -> Value {
    if v.is_finite() && v.fract() == 0.0 {
        Value::from(v as i64)
    } else {
        Value::from(v)
    }
}

/// `addBufferToUsage` — add the buffer to whichever input/total fields exist.
pub fn add_buffer_to_usage(usage: &Value) -> Value {
    let Some(obj) = usage.as_object() else {
        return usage.clone();
    };
    let mut result = obj.clone();

    if let Some(v) = result.get("input_tokens").and_then(as_number) {
        result.insert("input_tokens".into(), as_json_number(v + BUFFER_TOKENS as f64));
    }
    if let Some(v) = result.get("prompt_tokens").and_then(as_number) {
        result.insert("prompt_tokens".into(), as_json_number(v + BUFFER_TOKENS as f64));
    }
    if let Some(v) = result.get("total_tokens").and_then(as_number) {
        result.insert("total_tokens".into(), as_json_number(v + BUFFER_TOKENS as f64));
    } else if let (Some(p), Some(c)) = (
        result.get("prompt_tokens").and_then(as_number),
        result.get("completion_tokens").and_then(as_number),
    ) {
        // JS adds the buffer to prompt_tokens above, then totals the buffered prompt.
        result.insert("total_tokens".into(), as_json_number(p + c));
    }

    Value::Object(result)
}

/// `filterUsageForFormat` — keep only the fields the target wire format accepts.
pub fn filter_usage_for_format(usage: &Value, target_format: &str) -> Value {
    let Some(obj) = usage.as_object() else {
        return usage.clone();
    };

    const CLAUDE_FIELDS: &[&str] = &[
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
        "estimated",
    ];
    const GEMINI_FIELDS: &[&str] = &[
        "promptTokenCount",
        "candidatesTokenCount",
        "totalTokenCount",
        "cachedContentTokenCount",
        "thoughtsTokenCount",
        "estimated",
    ];
    const OPENAI_RESPONSES_FIELDS: &[&str] = &[
        "input_tokens",
        "output_tokens",
        "input_tokens_details",
        "output_tokens_details",
        "estimated",
    ];
    const DEFAULT_FIELDS: &[&str] = &[
        "prompt_tokens",
        "completion_tokens",
        "total_tokens",
        "cached_tokens",
        "reasoning_tokens",
        "prompt_tokens_details",
        "completion_tokens_details",
        "estimated",
    ];

    let fields: &[&str] = if target_format == format::CLAUDE {
        CLAUDE_FIELDS
    } else if target_format == format::GEMINI {
        GEMINI_FIELDS
    } else if target_format == format::OPENAI_RESPONSES {
        OPENAI_RESPONSES_FIELDS
    } else if target_format == format::GEMINI_CLI || target_format == format::ANTIGRAVITY {
        GEMINI_FIELDS
    } else if target_format == format::OPENAI_RESPONSE {
        OPENAI_RESPONSES_FIELDS
    } else {
        DEFAULT_FIELDS
    };

    let mut filtered = serde_json::Map::new();
    for field in fields {
        if let Some(v) = obj.get(*field) {
            filtered.insert((*field).to_string(), v.clone());
        }
    }
    Value::Object(filtered)
}

/// `normalizeUsage` — coerce every known token field to a number; `None` when empty.
pub fn normalize_usage(usage: &Value) -> Option<Value> {
    let obj = usage.as_object()?;

    let mut normalized = serde_json::Map::new();
    let mut assign_number = |key: &str| {
        if let Some(v) = obj.get(key) {
            if let Some(f) = as_number(v) {
                if f.is_finite() {
                    normalized.insert(key.to_string(), as_json_number(f));
                }
            }
        }
    };
    for key in [
        "prompt_tokens",
        "completion_tokens",
        "total_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
        "cached_tokens",
        "reasoning_tokens",
    ] {
        assign_number(key);
    }

    // Preserve nested details objects for OpenAI format forwarding.
    for key in ["prompt_tokens_details", "completion_tokens_details"] {
        if let Some(v) = obj.get(key) {
            if v.is_object() {
                normalized.insert(key.to_string(), v.clone());
            }
        }
    }

    if normalized.is_empty() {
        return None;
    }
    Some(Value::Object(normalized))
}

/// `canonicalizeUsage` — one storage/cost convention across providers:
///   prompt_tokens               = total input INCLUDING cache read + cache creation
///   cached_tokens               = cache-read portion (subset of prompt_tokens)
///   cache_creation_input_tokens = cache-write portion (subset of prompt_tokens)
///   completion_tokens, reasoning_tokens, total_tokens
///
/// Discriminator: Claude reports cache_read_input_tokens with a prompt that EXCLUDES
/// cache, so we fold cache into prompt. OpenAI/Gemini report cached_tokens already
/// counted inside prompt, so we pass through. Idempotent: once folded the output
/// carries cached_tokens (not cache_read_input_tokens), so re-running takes the
/// passthrough branch and does not double-add.
pub fn canonicalize_usage(usage: &Value) -> Option<Value> {
    let obj = usage.as_object()?;

    let completion = num_or_zero(obj.get("completion_tokens").or_else(|| obj.get("output_tokens")));
    let reasoning = num_or_zero(obj.get("reasoning_tokens"));
    // Fall back to the nested prompt_tokens_details.cache_creation_tokens shape
    // (buildUsage()'s OpenAI-forwarding format) when the top-level field is absent.
    let cache_creation = num_or_zero(
        obj.get("cache_creation_input_tokens")
            .or_else(|| obj.get("prompt_tokens_details").and_then(|d| d.get("cache_creation_tokens"))),
    );

    let mut prompt = num_or_zero(obj.get("prompt_tokens").or_else(|| obj.get("input_tokens")));
    let cached;

    // Claude path: prompt excludes cache; cache_read_input_tokens and/or
    // cache_creation_input_tokens are separate. A cache-miss "first write" only
    // carries cache_creation_input_tokens (no cache_read_input_tokens yet), so check
    // both fields. Guard on the absence of `cached_tokens`: our own canonical output
    // always sets that key (even to 0), so re-running on an already-folded result
    // takes the passthrough branch instead of folding again.
    if !obj.contains_key("cached_tokens")
        && (obj.contains_key("cache_read_input_tokens") || obj.contains_key("cache_creation_input_tokens"))
    {
        cached = num_or_zero(obj.get("cache_read_input_tokens"));
        prompt = prompt + cached + cache_creation;
    } else {
        // OpenAI/Gemini path (or already-canonical input): prompt already includes
        // cached_tokens. Mirror the cacheCreation fallback above: buildUsage() only
        // ever emits the nested prompt_tokens_details.cached_tokens shape.
        cached = num_or_zero(
            obj.get("cached_tokens")
                .or_else(|| obj.get("prompt_tokens_details").and_then(|d| d.get("cached_tokens"))),
        );
    }

    let mut result = serde_json::Map::new();
    result.insert("prompt_tokens".into(), as_json_number(prompt as f64));
    result.insert("completion_tokens".into(), as_json_number(completion as f64));
    // Recompute rather than pass through: when the fold branch ran above, an
    // upstream total_tokens (cache-exclusive) would otherwise be stale.
    result.insert("total_tokens".into(), as_json_number((prompt + completion) as f64));
    result.insert("cached_tokens".into(), as_json_number(cached as f64));
    result.insert("cache_creation_input_tokens".into(), as_json_number(cache_creation as f64));
    if reasoning > 0 {
        result.insert("reasoning_tokens".into(), as_json_number(reasoning as f64));
    }
    Some(Value::Object(result))
}

/// `hasValidUsage` — true when at least one known token field is a number > 0.
pub fn has_valid_usage(usage: &Value) -> bool {
    const TOKEN_FIELDS: &[&str] = &[
        "prompt_tokens",
        "completion_tokens",
        "total_tokens",
        "input_tokens",
        "output_tokens",
        "promptTokenCount",
        "candidatesTokenCount",
    ];
    TOKEN_FIELDS
        .iter()
        .any(|f| usage.get(*f).and_then(Value::as_f64).is_some_and(|n| n > 0.0))
}

/// `extractUsage` — pull usage out of any supported chunk shape.
pub fn extract_usage(chunk: &Value) -> Option<Value> {
    if !chunk.is_object() {
        return None;
    }
    let chunk_type = chunk.get("type").and_then(Value::as_str).unwrap_or_default();

    // Claude format (message_start event): carries input_tokens + cache_read +
    // cache_creation. message_delta later carries only the final output_tokens, so
    // callers must MERGE (merge_usage), not overwrite, to keep cache counts.
    if chunk_type == "message_start" {
        if let Some(u) = chunk.get("message").and_then(|m| m.get("usage")).filter(|u| u.is_object()) {
            return normalize_usage(&json_obj(vec![
                ("prompt_tokens", Value::from(num_or_zero(u.get("input_tokens")))),
                ("completion_tokens", Value::from(num_or_zero(u.get("output_tokens")))),
                ("cache_read_input_tokens", u.get("cache_read_input_tokens").cloned().unwrap_or(Value::Null)),
                ("cache_creation_input_tokens", u.get("cache_creation_input_tokens").cloned().unwrap_or(Value::Null)),
            ]));
        }
    }

    // Claude format (message_delta event)
    if chunk_type == "message_delta" {
        if let Some(u) = chunk.get("usage").filter(|u| u.is_object()) {
            return normalize_usage(&json_obj(vec![
                ("prompt_tokens", Value::from(num_or_zero(u.get("input_tokens")))),
                ("completion_tokens", Value::from(num_or_zero(u.get("output_tokens")))),
                ("cache_read_input_tokens", u.get("cache_read_input_tokens").cloned().unwrap_or(Value::Null)),
                ("cache_creation_input_tokens", u.get("cache_creation_input_tokens").cloned().unwrap_or(Value::Null)),
            ]));
        }
    }

    // OpenAI Responses API format (response.completed or response.done)
    if chunk_type == "response.completed" || chunk_type == "response.done" {
        if let Some(usage) = chunk.get("response").and_then(|r| r.get("usage")).filter(|u| u.is_object()) {
            let cached_tokens = usage
                .get("input_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .cloned();
            let cached_present = cached_tokens.as_ref().is_some_and(nr_core::json::truthy);
            return normalize_usage(&json_obj(vec![
                (
                    "prompt_tokens",
                    Value::from(num_or_zero(usage.get("input_tokens").or_else(|| usage.get("prompt_tokens")))),
                ),
                (
                    "completion_tokens",
                    Value::from(num_or_zero(
                        usage.get("output_tokens").or_else(|| usage.get("completion_tokens")),
                    )),
                ),
                ("cached_tokens", cached_tokens.unwrap_or(Value::Null)),
                (
                    "reasoning_tokens",
                    usage
                        .get("output_tokens_details")
                        .and_then(|d| d.get("reasoning_tokens"))
                        .cloned()
                        .unwrap_or(Value::Null),
                ),
                (
                    "prompt_tokens_details",
                    if cached_present {
                        usage.get("input_tokens_details").cloned().unwrap_or(Value::Null)
                    } else {
                        Value::Null
                    },
                ),
            ]));
        }
    }

    // OpenAI format (also covers DeepSeek which uses prompt_cache_hit_tokens)
    if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
        if usage.get("prompt_tokens").is_some() {
            return normalize_usage(&json_obj(vec![
                ("prompt_tokens", usage.get("prompt_tokens").cloned().unwrap_or(Value::Null)),
                ("completion_tokens", usage.get("completion_tokens").cloned().unwrap_or(Value::Null)),
                (
                    "cached_tokens",
                    usage
                        .get("prompt_tokens_details")
                        .and_then(|d| d.get("cached_tokens"))
                        .filter(|v| nr_core::json::truthy(v))
                        .or_else(|| usage.get("prompt_cache_hit_tokens"))
                        .cloned()
                        .unwrap_or(Value::Null),
                ),
                (
                    "reasoning_tokens",
                    usage
                        .get("completion_tokens_details")
                        .and_then(|d| d.get("reasoning_tokens"))
                        .cloned()
                        .unwrap_or(Value::Null),
                ),
                (
                    "prompt_tokens_details",
                    usage.get("prompt_tokens_details").cloned().unwrap_or(Value::Null),
                ),
                (
                    "completion_tokens_details",
                    usage.get("completion_tokens_details").cloned().unwrap_or(Value::Null),
                ),
            ]));
        }
    }

    // Gemini format (Antigravity). Antigravity wraps usageMetadata inside
    // `response`: { response: { usageMetadata: {...} } }
    let usage_meta = chunk
        .get("usageMetadata")
        .or_else(|| chunk.get("response").and_then(|r| r.get("usageMetadata")));
    if let Some(meta) = usage_meta.filter(|m| m.is_object()) {
        return normalize_usage(&json_obj(vec![
            ("prompt_tokens", Value::from(num_or_zero(meta.get("promptTokenCount")))),
            ("completion_tokens", Value::from(num_or_zero(meta.get("candidatesTokenCount")))),
            ("total_tokens", meta.get("totalTokenCount").cloned().unwrap_or(Value::Null)),
            ("cached_tokens", meta.get("cachedContentTokenCount").cloned().unwrap_or(Value::Null)),
            ("reasoning_tokens", meta.get("thoughtsTokenCount").cloned().unwrap_or(Value::Null)),
        ]));
    }

    // Ollama NDJSON format (raw from provider, before translation)
    // {"model":"...","done":true,"prompt_eval_count":N,"eval_count":M,"prompt_eval_cached_count":C}
    // prompt_eval_cached_count is a cache-read subset of prompt_eval_count (cache-INCLUSIVE).
    if chunk.get("done").and_then(Value::as_bool) == Some(true) {
        if let Some(prompt_eval) = chunk.get("prompt_eval_count").and_then(Value::as_f64) {
            let prompt = prompt_eval as i64;
            let completion = num_or_zero(chunk.get("eval_count"));
            return normalize_usage(&json_obj(vec![
                ("prompt_tokens", Value::from(prompt)),
                ("completion_tokens", Value::from(completion)),
                ("total_tokens", Value::from(prompt + completion)),
                ("cached_tokens", Value::from(num_or_zero(chunk.get("prompt_eval_cached_count")))),
            ]));
        }
    }

    None
}

fn json_obj(pairs: Vec<(&str, Value)>) -> Value {
    let mut map = serde_json::Map::new();
    for (k, v) in pairs {
        if !v.is_null() {
            map.insert(k.to_string(), v);
        }
    }
    Value::Object(map)
}

/// Field-wise max-merge of two usage objects. Anthropic splits usage across events:
/// message_start has real input+cache (output is a placeholder 1), message_delta has
/// the real cumulative output (input/cache absent). Max keeps the meaningful value
/// from each without clobbering. Idempotent for other providers that emit a single
/// complete usage object.
pub fn merge_usage(prev: Option<&Value>, next: Option<&Value>) -> Option<Value> {
    let prev = match prev {
        Some(p) => p,
        None => return next.cloned(),
    };
    let next = match next {
        Some(n) => n,
        None => return Some(prev.clone()),
    };
    let mut merged = match prev.as_object() {
        Some(o) => o.clone(),
        None => return Some(next.clone()),
    };

    if let Some(next_obj) = next.as_object() {
        for (k, v) in next_obj {
            if is_numberish(v) {
                let prev_num = merged.get(k).and_then(as_number).unwrap_or(0.0);
                let next_num = as_number(v).unwrap_or(0.0);
                merged.insert(k.clone(), as_json_number(prev_num.max(next_num)));
            } else if nr_core::json::truthy(v) {
                // nested details objects: take latest
                merged.insert(k.clone(), v.clone());
            }
        }
    }
    Some(Value::Object(merged))
}

/// `estimateInputTokens` — ~4 chars per token over the whole serialized body.
pub fn estimate_input_tokens(body: &Value) -> i64 {
    if !body.is_object() {
        return 0;
    }
    let Ok(s) = serde_json::to_string(body) else { return 0 };
    (s.chars().count() as i64 + 3) / 4
}

/// `estimateOutputTokens` — floor(len / 4), at least 1 when non-empty.
pub fn estimate_output_tokens(content_length: i64) -> i64 {
    if content_length <= 0 {
        return 0;
    }
    (content_length / 4).max(1)
}

/// `formatUsage` — build a usage object for the target wire format, buffered.
pub fn format_usage(input_tokens: i64, output_tokens: i64, target_format: &str) -> Value {
    if target_format == format::CLAUDE {
        return add_buffer_to_usage(&json!({
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "estimated": true,
        }));
    }
    add_buffer_to_usage(&json!({
        "prompt_tokens": input_tokens,
        "completion_tokens": output_tokens,
        "total_tokens": input_tokens + output_tokens,
        "estimated": true,
    }))
}

/// `estimateUsage` — estimate a whole usage object when the provider sends none.
pub fn estimate_usage(body: &Value, content_length: i64, target_format: &str) -> Value {
    format_usage(
        estimate_input_tokens(body),
        estimate_output_tokens(content_length),
        target_format,
    )
}

/// `logUsage` — legacy per-chunk console line, gated behind `LOG_USAGE_VERBOSE=1`.
pub fn log_usage(
    provider: Option<&str>,
    usage: &Value,
    _model: Option<&str>,
    connection_id: Option<&str>,
    _api_key: Option<&str>,
) {
    if !usage.is_object() || !debug_usage_enabled() {
        return;
    }
    let p = provider.map(str::to_uppercase).unwrap_or_else(|| "UNKNOWN".to_string());
    let in_tokens = num_or_zero(usage.get("prompt_tokens").or_else(|| usage.get("input_tokens")));
    let out_tokens = num_or_zero(usage.get("completion_tokens").or_else(|| usage.get("output_tokens")));
    let account = connection_id
        .map(|id| format!("{}...", &id[..id.len().min(8)]))
        .unwrap_or_else(|| "unknown".to_string());
    tracing::info!("[USAGE] {p} | in={in_tokens} | out={out_tokens} | account={account}");
}

use serde_json::json;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_adds_to_openai_and_claude() {
        let openai = add_buffer_to_usage(&json!({"prompt_tokens": 10, "completion_tokens": 5}));
        assert_eq!(openai["prompt_tokens"], 2010);
        assert_eq!(openai["completion_tokens"], 5);
        assert_eq!(openai["total_tokens"], 2015);

        let claude = add_buffer_to_usage(&json!({"input_tokens": 10, "output_tokens": 5}));
        assert_eq!(claude["input_tokens"], 2010);
        assert!(claude.get("total_tokens").is_none());
    }

    #[test]
    fn filter_keeps_only_format_fields() {
        let usage = json!({
            "prompt_tokens": 1, "output_tokens": 2, "reasoning_tokens": 3,
            "cache_read_input_tokens": 4, "estimated": true,
        });
        let claude = filter_usage_for_format(&usage, format::CLAUDE);
        assert_eq!(claude, json!({"output_tokens": 2, "cache_read_input_tokens": 4, "estimated": true}));
        let gemini = filter_usage_for_format(&usage, format::GEMINI_CLI);
        assert_eq!(gemini, json!({"estimated": true}));
        let default = filter_usage_for_format(&usage, "kiro");
        assert_eq!(default, json!({"prompt_tokens": 1, "reasoning_tokens": 3, "estimated": true}));
    }

    #[test]
    fn normalize_coerces_and_rejects_empty() {
        let n = normalize_usage(&json!({"prompt_tokens": "12", "cached_tokens": 3, "junk": 9})).unwrap();
        assert_eq!(n["prompt_tokens"], 12);
        assert_eq!(n["cached_tokens"], 3);
        assert!(n.get("junk").is_none());
        assert!(normalize_usage(&json!({})).is_none());
        assert!(normalize_usage(&json!([1, 2])).is_none());
    }

    #[test]
    fn canonicalize_folds_claude_cache() {
        let c = canonicalize_usage(&json!({
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "cache_read_input_tokens": 900,
            "cache_creation_input_tokens": 20,
        }))
        .unwrap();
        assert_eq!(c["prompt_tokens"], 1020);
        assert_eq!(c["cached_tokens"], 900);
        assert_eq!(c["cache_creation_input_tokens"], 20);
        assert_eq!(c["total_tokens"], 1070);
    }

    #[test]
    fn canonicalize_is_idempotent() {
        let first = canonicalize_usage(&json!({
            "prompt_tokens": 100, "completion_tokens": 10, "cache_read_input_tokens": 50,
        }))
        .unwrap();
        let second = canonicalize_usage(&first).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn canonicalize_passthrough_openai() {
        let c = canonicalize_usage(&json!({
            "prompt_tokens": 100, "completion_tokens": 10, "cached_tokens": 40,
        }))
        .unwrap();
        assert_eq!(c["prompt_tokens"], 100);
        assert_eq!(c["cached_tokens"], 40);
        assert!(c.get("reasoning_tokens").is_none());
    }

    #[test]
    fn canonicalize_first_write_without_cache_read() {
        let c = canonicalize_usage(&json!({
            "prompt_tokens": 5, "cache_creation_input_tokens": 7,
        }))
        .unwrap();
        assert_eq!(c["prompt_tokens"], 12);
        assert_eq!(c["cached_tokens"], 0);
    }

    #[test]
    fn extract_claude_message_start_and_delta() {
        let start = extract_usage(&json!({
            "type": "message_start",
            "message": {"usage": {"input_tokens": 12, "output_tokens": 1, "cache_read_input_tokens": 30}},
        }))
        .unwrap();
        assert_eq!(start["prompt_tokens"], 12);
        assert_eq!(start["cache_read_input_tokens"], 30);

        let delta = extract_usage(&json!({
            "type": "message_delta",
            "usage": {"output_tokens": 40},
        }))
        .unwrap();
        assert_eq!(delta["completion_tokens"], 40);
    }

    #[test]
    fn extract_openai_and_deepseek_cache() {
        let u = extract_usage(&json!({"usage": {"prompt_tokens": 9, "completion_tokens": 3}})).unwrap();
        assert_eq!(u["prompt_tokens"], 9);

        let ds = extract_usage(&json!({"usage": {"prompt_tokens": 9, "prompt_cache_hit_tokens": 4}})).unwrap();
        assert_eq!(ds["cached_tokens"], 4);
    }

    #[test]
    fn extract_responses_api() {
        let u = extract_usage(&json!({
            "type": "response.completed",
            "response": {"usage": {
                "input_tokens": 20, "output_tokens": 8,
                "input_tokens_details": {"cached_tokens": 5},
                "output_tokens_details": {"reasoning_tokens": 2},
            }},
        }))
        .unwrap();
        assert_eq!(u["prompt_tokens"], 20);
        assert_eq!(u["cached_tokens"], 5);
        assert_eq!(u["reasoning_tokens"], 2);
    }

    #[test]
    fn extract_gemini_antigravity_and_ollama() {
        let g = extract_usage(&json!({
            "response": {"usageMetadata": {
                "promptTokenCount": 11, "candidatesTokenCount": 7,
                "totalTokenCount": 18, "thoughtsTokenCount": 3,
            }},
        }))
        .unwrap();
        assert_eq!(g["prompt_tokens"], 11);
        assert_eq!(g["reasoning_tokens"], 3);

        let o = extract_usage(&json!({
            "done": true, "prompt_eval_count": 10, "eval_count": 6, "prompt_eval_cached_count": 2,
        }))
        .unwrap();
        assert_eq!(o["total_tokens"], 16);
        assert_eq!(o["cached_tokens"], 2);
    }

    #[test]
    fn merge_takes_fieldwise_max_and_latest_details() {
        let a = json!({"prompt_tokens": 100, "completion_tokens": 1, "prompt_tokens_details": {"cached_tokens": 9}});
        let b = json!({"completion_tokens": 40});
        let m = merge_usage(Some(&a), Some(&b)).unwrap();
        assert_eq!(m["prompt_tokens"], 100);
        assert_eq!(m["completion_tokens"], 40);
        assert_eq!(m["prompt_tokens_details"]["cached_tokens"], 9);

        let latest = json!({"prompt_tokens_details": {"cached_tokens": 1}});
        let m2 = merge_usage(Some(&a), Some(&latest)).unwrap();
        assert_eq!(m2["prompt_tokens_details"]["cached_tokens"], 1);

        assert_eq!(merge_usage(None, Some(&b)), Some(b.clone()));
        assert_eq!(merge_usage(Some(&a), None), Some(a));
    }

    #[test]
    fn estimates_and_format() {
        assert_eq!(estimate_input_tokens(&json!({"a": 1})), 2); // "{\"a\":1}" = 7 chars
        assert_eq!(estimate_output_tokens(0), 0);
        assert_eq!(estimate_output_tokens(1), 1);
        assert_eq!(estimate_output_tokens(40), 10);

        let claude = format_usage(100, 10, format::CLAUDE);
        assert_eq!(claude["input_tokens"], 2100);
        assert_eq!(claude["estimated"], true);
        let openai = format_usage(100, 10, format::OPENAI);
        assert_eq!(openai["total_tokens"], 2110);

        let est = estimate_usage(&json!({"m": "x"}), 8, format::OPENAI);
        assert_eq!(est["completion_tokens"], 2);
        assert_eq!(est["prompt_tokens"], 2003);
    }

    #[test]
    fn has_valid_usage_requires_positive() {
        assert!(has_valid_usage(&json!({"prompt_tokens": 1})));
        assert!(has_valid_usage(&json!({"candidatesTokenCount": 3})));
        assert!(!has_valid_usage(&json!({"prompt_tokens": 0})));
        assert!(!has_valid_usage(&json!({"prompt_tokens": "5"})));
        assert!(!has_valid_usage(&json!({})));
    }
}
