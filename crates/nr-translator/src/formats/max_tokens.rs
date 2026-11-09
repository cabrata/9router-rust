//! Max tokens adjustment. Port of open-sse/translator/formats/maxTokens.js.

use nr_core::Json;

/// Adjust max_tokens based on request context
/// * `body` - Request body
/// * `ceiling` - Upper bound for max_tokens.
pub fn adjust_max_tokens(body: &Json, ceiling: i64) -> i64 {
    let default_max_tokens = 64000;
    let default_min_tokens = 128;

    let mut max_tokens = body.get("max_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(default_max_tokens);

    // Auto-increase for tool calling to prevent truncated arguments
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        if !tools.is_empty() && max_tokens < default_min_tokens {
            max_tokens = default_min_tokens;
        }
    }

    // Ensure max_tokens > thinking.budget_tokens (Claude API requirement)
    if let Some(thinking) = body.get("thinking").and_then(|v| v.as_object()) {
        if let Some(budget) = thinking.get("budget_tokens").and_then(|v| v.as_i64()) {
            if max_tokens <= budget {
                max_tokens = budget + 1024;
            }
        }
    }

    // Never exceed the ceiling
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
    fn test_adjust_max_tokens_default() {
        let body = json!({});
        let result = adjust_max_tokens(&body, 64000);
        assert_eq!(result, 64000);
    }

    #[test]
    fn test_adjust_max_tokens_with_thinking() {
        let body = json!({
            "thinking": {
                "budget_tokens": 8192
            }
        });
        let result = adjust_max_tokens(&body, 128000);
        assert!(result > 8192);
    }
}
