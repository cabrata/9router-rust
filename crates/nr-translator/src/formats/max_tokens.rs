//! Max tokens adjustment. Port of open-sse/translator/formats/maxTokens.js.

use nr_core::Json;

fn default_max_tokens() -> i64 {
    nr_data::runtime_u64("DEFAULT_MAX_TOKENS") as i64
}

fn default_min_tokens() -> i64 {
    nr_data::runtime_u64("DEFAULT_MIN_TOKENS") as i64
}

/// Adjust max_tokens based on request context.
/// * `body` - Request body
/// * `ceiling` - Upper bound for max_tokens. Callers with model context (e.g.
///   openai-to-claude) pass the model's real maxOutput so high-output models
///   (Opus 4.8 = 128000) aren't pre-clamped to the conservative 64000 default
///   before the model-aware step sees them.
pub fn adjust_max_tokens(body: &Json, ceiling: Option<i64>) -> i64 {
    let mut max_tokens = body
        .get("max_tokens")
        .and_then(|v| v.as_i64())
        .or_else(|| body.get("max_completion_tokens").and_then(|v| v.as_i64()))
        .unwrap_or_else(default_max_tokens);

    // Auto-increase for tool calling to prevent truncated arguments (min never above max)
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        if !tools.is_empty() && max_tokens < default_min_tokens() {
            max_tokens = default_min_tokens();
        }
    }

    // Ensure max_tokens > thinking.budget_tokens (Claude API requirement).
    // Claude API requires strictly greater, so add buffer instead of using the
    // ceiling which could equal budget_tokens when budget_tokens >= ceiling.
    if let Some(budget) = body
        .get("thinking")
        .and_then(|t| t.get("budget_tokens"))
        .and_then(|v| v.as_i64())
    {
        if max_tokens <= budget {
            max_tokens = budget + 1024;
        }
    }

    // Never exceed the ceiling
    let ceiling = ceiling.unwrap_or_else(default_max_tokens);
    if max_tokens > ceiling {
        max_tokens = ceiling;
    }

    max_tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_default() {
        assert_eq!(adjust_max_tokens(&json!({}), None), 64000);
    }

    #[test]
    fn test_explicit_max_tokens() {
        assert_eq!(adjust_max_tokens(&json!({"max_tokens": 1000}), None), 1000);
    }

    #[test]
    fn test_tools_raise_to_min() {
        let body = json!({"max_tokens": 100, "tools": [{"type": "function"}]});
        assert_eq!(adjust_max_tokens(&body, None), 32000);
    }

    #[test]
    fn test_no_raise_without_tools() {
        let body = json!({"max_tokens": 100, "tools": []});
        assert_eq!(adjust_max_tokens(&body, None), 100);
    }

    #[test]
    fn test_thinking_budget_buffer() {
        let body = json!({"max_tokens": 8192, "thinking": {"budget_tokens": 8192}});
        assert_eq!(adjust_max_tokens(&body, None), 9216);
    }

    #[test]
    fn test_thinking_budget_clamped_by_ceiling() {
        let body = json!({"thinking": {"budget_tokens": 50000}});
        assert_eq!(adjust_max_tokens(&body, None), 64000);
        let body = json!({"thinking": {"budget_tokens": 50000}});
        assert_eq!(adjust_max_tokens(&body, Some(10000)), 10000);
    }

    #[test]
    fn test_model_ceiling_preserves_high_output() {
        let body = json!({"max_tokens": 100000});
        assert_eq!(adjust_max_tokens(&body, Some(128000)), 100000);
        assert_eq!(adjust_max_tokens(&body, None), 64000);
    }
}
