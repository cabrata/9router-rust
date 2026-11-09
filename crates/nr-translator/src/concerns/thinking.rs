//! Reasoning effort ↔ provider-native thinking config. Port of open-sse/translator/concerns/thinking.js.
//! Central source of truth for level↔budget maps (web-standard values).
//! Provider-specific application lives in thinkingUnified.rs; this file is maps-only.

use std::collections::HashMap;
use once_cell::sync::Lazy;

/// Discrete effort levels, ordered low→high.
pub const EFFORT_LEVELS: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max"];

/// Web-standard level → budget_tokens (Anthropic/Gemini docs).
pub static LEVEL_TO_BUDGET: Lazy<HashMap<&'static str, i64>> = Lazy::new(|| {
    let mut m = HashMap::new();
    m.insert("none", 0);
    m.insert("minimal", 512);
    m.insert("low", 1024);
    m.insert("medium", 8192);
    m.insert("high", 24576);
    m.insert("xhigh", 32768);
    m.insert("max", 128000);
    m
});

/// Returns budget_tokens for an effort level, or None if unknown.
/// 0 means "no thinking"; None means "effort not recognized".
pub fn effort_to_budget(effort: &str) -> Option<i64> {
    LEVEL_TO_BUDGET.get(effort.to_lowercase().as_str()).copied()
}

/// OpenAI reasoning_effort → Gemini thinkingLevel (gemini-3 enum: minimal|low|medium|high).
/// Gemini 3 cannot fully disable thinking; "none"/"off" map to "minimal".
pub fn effort_to_thinking_level(effort: &str) -> &'static str {
    let e = effort.to_lowercase().trim().to_string();
    match e.as_str() {
        "none" | "off" => "minimal",
        "xhigh" | "max" => "high",
        _ => &e,
    }
}

/// Numeric budget → nearest discrete level (reverse map via thresholds).
/// Returns None when budget <= 0 (no reasoning).
/// Thresholds are midpoints between LEVEL_TO_BUDGET values: max (128000) is
/// reachable, with the xhigh/max boundary at the 32768/128000 midpoint (80384).
pub fn budget_to_level(budget: i64) -> Option<&'static str> {
    if budget <= 0 {
        return None;
    }
    if budget <= 768 {
        Some("minimal")
    } else if budget <= 4096 {
        Some("low")
    } else if budget <= 16384 {
        Some("medium")
    } else if budget <= 28672 {
        Some("high")
    } else if budget <= 80384 {
        Some("xhigh")
    } else {
        Some("max")
    }
}

/// Gemini thinkingBudget (numeric) → OpenAI reasoning_effort (antigravity reverse map).
pub fn budget_to_effort(budget: i64) -> Option<&'static str> {
    if budget <= 0 {
        return None;
    }
    if budget <= 2048 {
        Some("low")
    } else if budget <= 16384 {
        Some("medium")
    } else {
        Some("high")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_effort_to_budget() {
        assert_eq!(effort_to_budget("none"), Some(0));
        assert_eq!(effort_to_budget("minimal"), Some(512));
        assert_eq!(effort_to_budget("low"), Some(1024));
        assert_eq!(effort_to_budget("medium"), Some(8192));
        assert_eq!(effort_to_budget("high"), Some(24576));
    }

    #[test]
    fn test_effort_to_thinking_level() {
        assert_eq!(effort_to_thinking_level("none"), "minimal");
        assert_eq!(effort_to_thinking_level("xhigh"), "high");
        assert_eq!(effort_to_thinking_level("low"), "low");
    }

    #[test]
    fn test_budget_to_level() {
        assert_eq!(budget_to_level(0), None);
        assert_eq!(budget_to_level(512), Some("minimal"));
        assert_eq!(budget_to_level(8192), Some("medium"));
        assert_eq!(budget_to_level(128000), Some("max"));
    }
}
