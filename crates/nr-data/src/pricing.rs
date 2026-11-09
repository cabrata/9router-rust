use nr_core::{Json, JsonMap};
use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;

/// Resolve FREE_MODEL_NAMESPACES from dump
fn free_model_namespaces() -> &'static [&'static str] {
    export("open-sse/providers/pricing", "FREE_MODEL_NAMESPACES")
        .and_then(|v| v.as_array())
        .map(|arr| {
            Box::leak(
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
            ) as &[&str]
        })
        .unwrap_or(&[])
}

/// Resolve ZERO_PRICING from dump
fn zero_pricing() -> Json {
    export("open-sse/providers/pricing", "ZERO_PRICING")
        .cloned()
        .unwrap_or_else(|| {
            serde_json::json!({
                "input": 0,
                "output": 0,
                "cached": 0,
                "reasoning": 0,
                "cache_creation": 0
            })
        })
}

/// Resolve MODEL_PRICING from dump
fn model_pricing() -> &'static JsonMap {
    static PRICING: LazyLock<JsonMap> = LazyLock::new(|| {
        export("open-sse/providers/pricing", "MODEL_PRICING")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default()
    });
    &PRICING
}

/// Resolve PROVIDER_PRICING from dump
fn provider_pricing() -> &'static JsonMap {
    static PRICING: LazyLock<JsonMap> = LazyLock::new(|| {
        export("open-sse/providers/pricing", "PROVIDER_PRICING")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default()
    });
    &PRICING
}

/// Resolve PATTERN_PRICING from dump
fn pattern_pricing() -> &'static [Json] {
    static PATTERNS: LazyLock<Vec<Json>> = LazyLock::new(|| {
        export("open-sse/providers/pricing", "PATTERN_PRICING")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
    });
    &PATTERNS
}

fn export(path: &str, name: &str) -> Option<&'static Json> {
    crate::export(path, name)
}

/// True when the model id sits in a namespace upstream bills at $0.
pub fn is_free_model(model: Option<&str>) -> bool {
    let model = match model {
        Some(m) => m,
        None => return false,
    };

    let lower = model.to_lowercase();
    for ns in free_model_namespaces() {
        if lower.starts_with(ns) {
            return true;
        }
    }
    false
}

/// Match a model ID against a glob pattern (* = wildcard). Case-insensitive.
///
/// Equivalent to the JS `globToRegex(pattern).test(model)`, but this runs for
/// every catalog entry on `/api/models` (1491 models x 164 patterns), so
/// compiling a regex per call made that route take >15s. A direct
/// two-pointer glob matcher is exact, allocation-free and fast.
pub fn match_pattern(pattern: &str, model: Option<&str>) -> bool {
    let model = match model {
        Some(m) => m,
        None => return false,
    };
    glob_match_ci(pattern, model)
}

/// Case-insensitive glob match where `*` matches any (possibly empty) run.
fn glob_match_ci(pattern: &str, text: &str) -> bool {
    let p = pattern.as_bytes();
    let t = text.as_bytes();
    let (mut pi, mut ti) = (0usize, 0usize);
    // Backtrack point: the pattern index just after the last `*` and the text
    // position it was first tried against.
    let mut star_p: Option<usize> = None;
    let mut star_t = 0usize;

    while ti < t.len() {
        if pi < p.len() && p[pi] != b'*' && p[pi].eq_ignore_ascii_case(&t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star_p = Some(pi + 1);
            star_t = ti;
            pi += 1;
        } else if let Some(sp) = star_p {
            // Mismatch after a `*`: let the star consume one more character.
            pi = sp;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// Resolve pricing for a model using the 4-step fallback chain:
///   1. PROVIDER_PRICING[provider][model]
///   2. free namespace (upstream bills $0)
///   3. MODEL_PRICING[model]
///   4. PATTERN_PRICING (glob match)
pub fn get_pricing_for_model(provider: Option<&str>, model: Option<&str>) -> Option<Json> {
    let model = match model {
        Some(m) => m,
        None => return None,
    };

    // 1. Provider-specific override
    if let Some(prov) = provider {
        if let Some(prov_map) = provider_pricing().get(prov).and_then(|p| p.as_object()) {
            if let Some(pricing) = prov_map.get(model) {
                return Some(pricing.clone());
            }
        }
    }

    // 2. Free namespaces bill $0
    if is_free_model(Some(model)) {
        return Some(zero_pricing());
    }

    // 3. Canonical model pricing (strip vendor prefix if needed)
    let base_model = if let Some(idx) = model.find('/') {
        &model[idx + 1..]
    } else {
        model
    };

    if let Some(pricing) = model_pricing().get(base_model) {
        return Some(pricing.clone());
    }
    if let Some(pricing) = model_pricing().get(model) {
        return Some(pricing.clone());
    }

    // 4. Pattern match
    for pattern_entry in pattern_pricing() {
        if let Some(pattern) = pattern_entry.get("pattern").and_then(Json::as_str) {
            if match_pattern(pattern, Some(base_model)) || match_pattern(pattern, Some(model)) {
                if let Some(pricing) = pattern_entry.get("pricing") {
                    return Some(pricing.clone());
                }
            }
        }
    }

    None
}

/// Get all provider pricing (for UI / API).
pub fn get_default_pricing() -> Json {
    export("open-sse/providers/pricing", "PROVIDER_PRICING")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}))
}

/// Format cost for display
pub fn format_cost(cost: Option<f64>) -> String {
    match cost {
        Some(c) if c.is_finite() => format!("${:.2}", c),
        _ => "$0.00".to_string(),
    }
}

/// Calculate cost from tokens and pricing
pub fn calculate_cost_from_tokens(tokens: Option<&Json>, pricing: Option<&Json>) -> f64 {
    let tokens = match tokens {
        Some(t) => t,
        None => return 0.0,
    };
    let pricing = match pricing {
        Some(p) => p,
        None => return 0.0,
    };

    let mut cost = 0.0;

    let input_tokens = tokens
        .get("prompt_tokens")
        .or_else(|| tokens.get("input_tokens"))
        .and_then(Json::as_f64)
        .unwrap_or(0.0);

    let cached_tokens = tokens
        .get("cached_tokens")
        .or_else(|| tokens.get("cache_read_input_tokens"))
        .and_then(Json::as_f64)
        .unwrap_or(0.0);

    let cache_creation_tokens = tokens
        .get("cache_creation_input_tokens")
        .and_then(Json::as_f64)
        .unwrap_or(0.0);

    let non_cached_input = (input_tokens - cached_tokens - cache_creation_tokens).max(0.0);

    let input_price = pricing
        .get("input")
        .and_then(Json::as_f64)
        .unwrap_or(0.0);
    cost += non_cached_input * (input_price / 1_000_000.0);

    if cached_tokens > 0.0 {
        let cached_price = pricing
            .get("cached")
            .or_else(|| pricing.get("input"))
            .and_then(Json::as_f64)
            .unwrap_or(0.0);
        cost += cached_tokens * (cached_price / 1_000_000.0);
    }

    let output_tokens = tokens
        .get("completion_tokens")
        .or_else(|| tokens.get("output_tokens"))
        .and_then(Json::as_f64)
        .unwrap_or(0.0);
    let output_price = pricing
        .get("output")
        .and_then(Json::as_f64)
        .unwrap_or(0.0);
    cost += output_tokens * (output_price / 1_000_000.0);

    let reasoning_tokens = tokens
        .get("reasoning_tokens")
        .and_then(Json::as_f64)
        .unwrap_or(0.0);
    if reasoning_tokens > 0.0 {
        let reasoning_price = pricing
            .get("reasoning")
            .or_else(|| pricing.get("output"))
            .and_then(Json::as_f64)
            .unwrap_or(0.0);
        cost += reasoning_tokens * (reasoning_price / 1_000_000.0);
    }

    if cache_creation_tokens > 0.0 {
        let cache_creation_price = pricing
            .get("cache_creation")
            .or_else(|| pricing.get("input"))
            .and_then(Json::as_f64)
            .unwrap_or(0.0);
        cost += cache_creation_tokens * (cache_creation_price / 1_000_000.0);
    }

    cost
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_free_model() {
        assert!(is_free_model(Some("cline-free/deepseek-v4")));
        assert!(!is_free_model(Some("deepseek-v4")));
        assert!(!is_free_model(None));
    }

    #[test]
    fn test_match_pattern() {
        assert!(match_pattern("*claude*", Some("claude-opus-4")));
        assert!(match_pattern("gpt-*", Some("gpt-4-turbo")));
        assert!(!match_pattern("gpt-*", Some("claude-opus")));
    }

    /// Glob semantics must match the previous regex build `^<escaped>$` with
    /// `*` -> `.*`, including case-insensitivity and edge cases.
    #[test]
    fn test_glob_edge_cases() {
        assert!(match_pattern("*", Some("anything")));
        assert!(match_pattern("**", Some("anything")));
        assert!(match_pattern("", Some("")));
        assert!(!match_pattern("", Some("x")));
        assert!(match_pattern("*-codex-high", Some("gpt-5-codex-high")));
        assert!(!match_pattern("*-codex-high", Some("gpt-5-codex-highx")));
        assert!(match_pattern("kimi-*-thinking", Some("kimi-k2-thinking")));
        assert!(match_pattern("*CLAUDE*", Some("claude-opus-4")));
        assert!(match_pattern("Gemini-*-Flash-Lite", Some("gemini-2.5-flash-lite")));
        // Regex metacharacters in the pattern are literal.
        assert!(match_pattern("a.b", Some("a.b")));
        assert!(!match_pattern("a.b", Some("axb")));
        assert!(match_pattern("x(y)", Some("x(y)")));
        assert!(!match_pattern("x(y)", Some("xy")));
        assert!(!match_pattern("*claude*", None));
    }

    /// Regression: `/api/models` calls this 1491 x 164 times; per-call regex
    /// compilation made the route take 15s.
    #[test]
    fn test_match_pattern_is_cheap() {
        let start = std::time::Instant::now();
        let mut hits = 0usize;
        for _ in 0..2_000 {
            for pat in ["*-codex-mini-*", "*claude*opus-5*", "gemini-*-flash-lite"] {
                if match_pattern(pat, Some("claude-opus-5-20250101")) {
                    hits += 1;
                }
            }
        }
        assert_eq!(hits, 2_000);
        assert!(
            start.elapsed() < std::time::Duration::from_millis(500),
            "glob matching too slow: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn test_format_cost() {
        assert_eq!(format_cost(Some(1.5)), "$1.50");
        assert_eq!(format_cost(Some(0.0)), "$0.00");
        assert_eq!(format_cost(None), "$0.00");
    }
}
