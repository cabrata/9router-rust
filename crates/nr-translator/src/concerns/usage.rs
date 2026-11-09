//! Usage token tracking. Port of open-sse/translator/concerns/usage.js.

use nr_core::Json;
use serde_json::json;

#[derive(Clone, Debug, Default)]
pub struct UsageParams {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub cached_tokens: i64,
    pub cache_creation_tokens: i64,
    pub reasoning_tokens: i64,
}

/// Build OpenAI usage object. Caller computes prompt/completion/total.
/// Optional details added only when > 0 (matches existing claude/gemini/codex behavior).
pub fn build_usage(params: &UsageParams) -> Json {
    let mut usage = json!({
        "prompt_tokens": params.prompt_tokens,
        "completion_tokens": params.completion_tokens,
        "total_tokens": params.total_tokens,
    });

    if params.cached_tokens > 0 || params.cache_creation_tokens > 0 {
        let mut details = serde_json::Map::new();
        if params.cached_tokens > 0 {
            details.insert("cached_tokens".to_string(), json!(params.cached_tokens));
        }
        if params.cache_creation_tokens > 0 {
            details.insert("cache_creation_tokens".to_string(), json!(params.cache_creation_tokens));
        }
        usage.as_object_mut().unwrap().insert("prompt_tokens_details".to_string(), Json::Object(details));
    }

    if params.reasoning_tokens > 0 {
        let details = json!({
            "reasoning_tokens": params.reasoning_tokens
        });
        usage.as_object_mut().unwrap().insert("completion_tokens_details".to_string(), details);
    }

    usage
}

fn n(v: &Json) -> i64 {
    v.as_i64().unwrap_or(0)
}

/// Convert Claude usage → OpenAI format
pub fn claude_usage(raw: &Json) -> UsageParams {
    let input = n(&raw["input_tokens"]);
    let output = n(&raw["output_tokens"]);
    let cache_read = n(&raw["cache_read_input_tokens"]);
    let cache_create = n(&raw["cache_creation_input_tokens"]);
    let prompt = input + cache_read + cache_create;

    UsageParams {
        prompt_tokens: prompt,
        completion_tokens: output,
        total_tokens: prompt + output,
        cached_tokens: cache_read,
        cache_creation_tokens: cache_create,
        reasoning_tokens: 0,
    }
}

/// Convert Gemini usage → OpenAI format
pub fn gemini_usage(raw: &Json) -> UsageParams {
    let cached = n(&raw["cachedContentTokenCount"]);
    let prompt = n(&raw["promptTokenCount"]);
    let thoughts = n(&raw["thoughtsTokenCount"]);
    let total = n(&raw["totalTokenCount"]);
    let mut candidates = n(&raw["candidatesTokenCount"]);

    // Fallback: derive candidates from total when upstream omits it
    if candidates == 0 && total > 0 {
        candidates = total - prompt - thoughts;
        if candidates < 0 {
            candidates = 0;
        }
    }

    UsageParams {
        prompt_tokens: prompt,
        completion_tokens: candidates + thoughts,
        total_tokens: total,
        cached_tokens: cached,
        cache_creation_tokens: 0,
        reasoning_tokens: thoughts,
    }
}

/// Convert Kiro usage → OpenAI format
pub fn kiro_usage(raw: &Json) -> UsageParams {
    let input = n(&raw["inputTokens"]);
    let output = n(&raw["outputTokens"]);
    let cached = n(&raw["cache_read_input_tokens"])
        .max(n(&raw["cachedTokens"]))
        .max(n(&raw["cached_tokens"]));
    let cache_creation = n(&raw["cache_creation_input_tokens"]);

    UsageParams {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: input + output,
        cached_tokens: cached,
        cache_creation_tokens: cache_creation,
        reasoning_tokens: 0,
    }
}

/// Convert Ollama usage → OpenAI format
pub fn ollama_usage(raw: &Json) -> UsageParams {
    let input = n(&raw["prompt_eval_count"]);
    let output = n(&raw["eval_count"]);
    let cached = n(&raw["prompt_eval_cached_count"]);

    UsageParams {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: input + output,
        cached_tokens: cached,
        cache_creation_tokens: 0,
        reasoning_tokens: 0,
    }
}

/// Convert CommandCode usage → OpenAI format
pub fn commandcode_usage(raw: &Json) -> UsageParams {
    let input = n(&raw["inputTokens"]);
    let output = n(&raw["outputTokens"]);
    let total = if let Some(t) = raw.get("totalTokens").and_then(|v| v.as_i64()) {
        t
    } else {
        input + output
    };
    let cached = n(&raw["inputTokenDetails"]["cacheReadTokens"]);
    let cache_create = n(&raw["inputTokenDetails"]["cacheWriteTokens"]);

    UsageParams {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: total,
        cached_tokens: cached,
        cache_creation_tokens: cache_create,
        reasoning_tokens: 0,
    }
}

/// Convert provider-native usage object → OpenAI usage. Returns None if no extractor/raw.
pub fn to_openai_usage(raw: &Json, kind: &str) -> Option<Json> {
    if !raw.is_object() {
        return None;
    }

    let params = match kind {
        "claude" => claude_usage(raw),
        "gemini" => gemini_usage(raw),
        "kiro" => kiro_usage(raw),
        "ollama" => ollama_usage(raw),
        "commandcode" => commandcode_usage(raw),
        _ => return None,
    };

    Some(build_usage(&params))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_usage_basic() {
        let params = UsageParams {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            ..Default::default()
        };
        let usage = build_usage(&params);
        assert_eq!(usage["prompt_tokens"], 100);
        assert_eq!(usage["completion_tokens"], 50);
        assert_eq!(usage["total_tokens"], 150);
        assert!(usage.get("prompt_tokens_details").is_none());
    }

    #[test]
    fn test_build_usage_with_cache() {
        let params = UsageParams {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            cached_tokens: 20,
            cache_creation_tokens: 10,
            ..Default::default()
        };
        let usage = build_usage(&params);
        assert!(usage.get("prompt_tokens_details").is_some());
        assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 20);
        assert_eq!(usage["prompt_tokens_details"]["cache_creation_tokens"], 10);
    }
}
