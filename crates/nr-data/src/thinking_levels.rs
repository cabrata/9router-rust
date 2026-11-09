//! Port of open-sse/providers/thinkingLevels.js.
//!
//! Resolve valid thinking levels per model (drives the UI level picker for the
//! "model(level)" suffix). Reuses capabilities.js for thinkingFormat/canDisable
//! so this file only maps format -> levels (DRY).

use nr_core::Json;

// Shared level sets (deduped), verified against provider docs + wire.
const L_BASE: &[&str] = &["none", "low", "medium", "high"]; // qwen, step, hunyuan, gemini-budget
const L_ON_OFF: &[&str] = &["none", "thinking"]; // zai (binary), minimax (adaptive)
const L_OPENAI: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh"]; // GPT-5.x / o-series (no max)
const L_LEVEL_MAX: &[&str] = &["none", "low", "medium", "high", "max"]; // kimi
const L_BUDGET_X: &[&str] = &["none", "low", "medium", "high", "xhigh", "max"]; // claude-budget/adaptive
const L_GEMINI: &[&str] = &["minimal", "low", "medium", "high"]; // gemini-3 thinkingLevel (no disable)
const L_HI_MAX: &[&str] = &["none", "high", "max"]; // deepseek (low/med->high, xhigh->max)

/// thinkingFormat -> valid selectable levels (source of truth for UI options).
const FORMAT_LEVELS: &[(&str, &[&str])] = &[
    ("openai", L_OPENAI),
    ("claude-adaptive", L_BUDGET_X),
    ("claude-budget", L_BUDGET_X),
    ("gemini-level", L_GEMINI),
    ("gemini-budget", L_BASE),
    ("zai", L_ON_OFF),
    ("qwen", L_BASE),
    ("kimi", L_LEVEL_MAX),
    ("deepseek", L_HI_MAX),
    ("commandcode", &["none", "low", "medium", "high", "xhigh", "max"]),
    ("minimax", L_ON_OFF),
    ("hunyuan", L_BASE),
    ("step", L_BASE),
    ("nous", L_BASE),
];

const CODEX_GPT_5_6_LEVELS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

// Opus/Sonnet 4.6 lack xhigh (Anthropic + Kiro docs) - keep the 4-level+max set.
const CLAUDE_NO_XHIGH: &[&str] = &["none", "low", "medium", "high", "max"];

/// Model-name pattern overrides (glob, first match wins) - more precise than the
/// format default. ORDER MATTERS: provider-qualified entries and dotted exact ids
/// must precede the unqualified globs they refine (#5186, live-probed sets).
const PATTERN_THINKING: &[(Option<&str>, &str, &[&str])] = &[
    // Gemini 4 (Argon) rejects "minimal" with 400 - LOW is the floor; cannot disable.
    (None, "*gemini-4*", &["low", "medium", "high"]),
    (None, "*claude*4.6*", CLAUDE_NO_XHIGH),
    (None, "*claude*4-6*", CLAUDE_NO_XHIGH),
    (Some("codex"), "*gpt-6*", CODEX_GPT_5_6_LEVELS),
    (Some("codex"), "*gpt-5.6-sol*", &["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"]),
    (Some("codex"), "*gpt-5.6-terra*", &["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"]),
    (Some("codex"), "*gpt-5.6-luna*", CODEX_GPT_5_6_LEVELS),
    // codex cannot disable thinking
    (None, "*codex*", &["low", "medium", "high", "xhigh"]),
    // OpenCode Free (oc) MiMo rides the OpenRouter-backed route and takes the full
    // OpenAI effort enum verbatim (probed live). Owns the entry ahead of Xiaomi's
    // own *mimo*v2.6* below, whose set would make max unreachable here.
    (Some("opencode"), "*mimo*v2.6*", &["none", "minimal", "low", "medium", "high", "xhigh", "max"]),
    (None, "*mimo*v2.6*", &["none", "low", "medium", "high", "xhigh"]),
    // mimo-v2.5-pro on opencode-go rejects reasoning_effort "max" (probed live); v2.5 accepts it.
    (None, "*mimo*v2.5-pro*", &["none", "low", "medium", "high", "xhigh"]),
    // DeepSeek v4.* (Alibaba MaaS, probed live): low|medium|high|xhigh|max all 200.
    // "none" is a 400 on the anthropic route but kept for the picker = disable.
    // The codebuddy-cn exact ids below MUST stay above this unqualified glob:
    // PATTERN_THINKING is first-match-wins, so a later provider-qualified entry
    // would never fire for dotted ids like deepseek-v4.1-flash.
    (Some("codebuddy-cn"), "deepseek-v4-pro", &["low", "high", "xhigh"]),
    (Some("codebuddy-cn"), "deepseek-v4.1-flash", &["low", "high", "max"]),
    // OpenCode Zen free tier - free id of the same v4.1-flash publishes low|high|max
    // and forwards the level verbatim (no low->high collapse). The generic
    // *deepseek-v4.* glob would advertise medium/xhigh, which this model does not serve.
    (None, "*deepseek-v4.1-flash-free*", &["low", "high", "max"]),
    (None, "*deepseek-v4.*", &["none", "low", "medium", "high", "xhigh", "max"]),
    // codebuddy-cn per-model effort sets - the server's product-config payload publishes
    // reasoning.supportedEfforts per model. NOTE: the chat endpoint accepts any level
    // you send, but values outside a model's set are silently clamped, so the declared
    // set stays authoritative for the picker. Models that publish no supportedEfforts
    // (glm-5.1 / kimi-k3-1 / minimax-m3) fall through to the openai format default.
    (Some("codebuddy-cn"), "glm-5.3", &["low", "high", "max"]),
    (Some("codebuddy-cn"), "glm-5.3-flash", &["low", "high", "max"]),
    (Some("codebuddy-cn"), "glm-5.2", &["high", "xhigh"]),
    (Some("codebuddy-cn"), "kimi-k2.8-preview", &["low", "medium", "high"]),
    (Some("codebuddy-cn"), "hy3", &["low", "high"]),
    (Some("codebuddy-cn"), "hy4-preview", &["high"]),
    // codebuddy-intl rides the same gateway catalog, so its deepseek levels match.
    (Some("codebuddy-intl"), "deepseek-v4*", &["low", "high", "xhigh"]),
    // MiniMax Code (mcode) - adaptive effort via output_config.effort. M3.1 always
    // thinks (no none); M3 is switchable with none/high only.
    (Some("minimax-code"), "MiniMax-M3.1*", &["low", "medium", "high", "xhigh", "max"]),
    (Some("minimax-code"), "MiniMax-M3", &["none", "high"]),
    (Some("minimax-code-global"), "MiniMax-M3.1*", &["low", "medium", "high", "xhigh", "max"]),
    (Some("minimax-code-global"), "MiniMax-M3", &["none", "high"]),
];

// OpenAI documents reasoning.effort "max" for GPT-5.6 and GPT-6 (#5186). The codex
// rows above already carry it; an OpenAI Compatible node serving the same ids gets
// the same set, otherwise the "openai" format list (no max) clamps max to xhigh.
const OPENAI_MAX_EFFORT_MODEL: &str = "(?:^|/)gpt-(?:5\\.6|6)(?:[.-]|$)";

fn is_openai_compatible_node(provider: Option<&str>) -> bool {
    provider.unwrap_or("").starts_with("openai-compatible-")
}

fn levels_of(set: &[&str]) -> Vec<String> {
    set.iter().map(|s| s.to_string()).collect()
}

/// Returns valid thinking levels for a model, or None when the model has no reasoning.
pub fn get_thinking_levels(provider: Option<&str>, model: Option<&str>) -> Option<Vec<String>> {
    if provider == Some("kiro") && crate::kiro_constants::resolve_kiro_effort_path(model).is_none() {
        return None;
    }
    let caps = crate::capabilities::get_capabilities_for_model(provider, model);
    if caps.get("reasoning").and_then(Json::as_bool) != Some(true) {
        return None;
    }

    let model = model.unwrap_or("");
    // Strip a trailing "model(level)" suffix before provider-model lookups.
    let base_id = strip_level_suffix(model);

    let model_levels = if provider == Some("codex") {
        crate::get_provider_models("cx")
            .iter()
            .find(|entry| entry.get("id").and_then(Json::as_str) == Some(base_id.as_str()))
            .and_then(|entry| entry.get("thinkingLevels"))
            .and_then(Json::as_array)
            .map(|levels| {
                levels
                    .iter()
                    .filter_map(Json::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .filter(|levels| !levels.is_empty())
    } else {
        None
    };

    let hit = PATTERN_THINKING
        .iter()
        .find(|(entry_provider, pattern, _)| {
            entry_provider.map(|p| p == provider.unwrap_or("")).unwrap_or(true)
                && crate::pricing::match_pattern(pattern, Some(model))
        })
        .map(|(_, _, levels)| *levels);

    let max_effort_hit = if is_openai_compatible_node(provider)
        && crate::pricing::match_pattern(OPENAI_MAX_EFFORT_MODEL, Some(&base_id))
    {
        Some(CODEX_GPT_5_6_LEVELS)
    } else {
        None
    };

    let thinking_format = caps.get("thinkingFormat").and_then(Json::as_str);
    let levels = model_levels
        .or_else(|| hit.map(levels_of))
        .or_else(|| max_effort_hit.map(levels_of))
        .or_else(|| {
            thinking_format
                .and_then(|fmt| FORMAT_LEVELS.iter().find(|(name, _)| *name == fmt))
                .map(|(_, levels)| levels_of(levels))
        })
        .unwrap_or_else(|| levels_of(L_BASE));

    if caps.get("thinkingCanDisable").and_then(Json::as_bool) == Some(false) {
        return Some(levels.into_iter().filter(|l| l != "none").collect());
    }
    Some(levels)
}

/// Strip a trailing "model(level)" suffix, e.g. "claude-4.6(high)" -> "claude-4.6".
fn strip_level_suffix(model: &str) -> String {
    match model.find('(') {
        Some(open) if model.ends_with(')') && open > 0 && !model[..open].contains('(') => {
            model[..open].trim().to_string()
        }
        _ => model.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_non_reasoning_model_returns_none() {
        // gpt-4 has no reasoning capability -> no thinking levels.
        assert_eq!(get_thinking_levels(Some("openai"), Some("gpt-4")), None);
    }

    #[test]
    fn test_reasoning_model_returns_levels() {
        let levels = get_thinking_levels(Some("openai"), Some("gpt-5"));
        assert!(levels.is_some());
        assert!(levels.unwrap().contains(&"high".to_string()));
    }

    #[test]
    fn test_kiro_unsupported_model_returns_none() {
        // Kiro only supports thinking on newer Claude/GPT models.
        assert_eq!(get_thinking_levels(Some("kiro"), Some("claude-3-haiku")), None);
    }

    #[test]
    fn test_claude_adaptive_set() {
        let levels = get_thinking_levels(Some("anthropic"), Some("claude-opus-4.7"));
        let levels = levels.expect("claude-opus-4.7 supports thinking");
        assert!(levels.contains(&"xhigh".to_string()));
        assert!(levels.contains(&"none".to_string()));
    }

    #[test]
    fn test_claude_no_xhigh_for_4_6() {
        let levels = get_thinking_levels(Some("anthropic"), Some("claude-sonnet-4.6"));
        let levels = levels.expect("claude-sonnet-4.6 supports thinking");
        assert!(!levels.contains(&"xhigh".to_string()));
        assert!(levels.contains(&"max".to_string()));
    }

    #[test]
    fn test_can_disable_false_strips_none() {
        // claude-opus-5.5 declares thinkingCanDisable:false -> "none" removed.
        let levels = get_thinking_levels(Some("anthropic"), Some("claude-opus-5.5"));
        let levels = levels.expect("thinking supported");
        assert!(!levels.contains(&"none".to_string()));
        assert!(levels.contains(&"max".to_string()));
    }

    #[test]
    fn test_codex_gpt_5_6_sol_includes_ultra() {
        let levels = get_thinking_levels(Some("codex"), Some("gpt-5.6-sol")).expect("levels");
        assert_eq!(levels.last().map(String::as_str), Some("ultra"));
    }

    #[test]
    fn test_minimax_code_m3_none_high() {
        let levels = get_thinking_levels(Some("minimax-code"), Some("MiniMax-M3")).expect("levels");
        assert_eq!(levels, vec!["none", "high"]);
    }

    #[test]
    fn test_strip_level_suffix() {
        assert_eq!(strip_level_suffix("claude-4.6(high)"), "claude-4.6");
        assert_eq!(strip_level_suffix("gpt-5"), "gpt-5");
    }
}
