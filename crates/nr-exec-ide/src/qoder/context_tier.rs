//! Port of open-sse/shared/qoder/contextTier.js — context-window tiers.
//!
//! Each Qoder model_config ships a `context_config` list (e.g. 200K / 400K /
//! 1M) while `max_input_tokens` only carries the tier the IDE currently has
//! selected (~180K by default). A qodercli-style client has no picker, so a
//! long Claude-Code session that grew past the default tier is rejected
//! upstream even though the model supports 1M.
//!
//! Emulate the IDE: estimate the prompt size, pick the smallest advertised
//! tier that fits (never below the model's current default), and mirror the
//! choice into the same three places the IDE writes.

use nr_core::Json;
use serde_json::json;

use super::constants::{
    QODER_CONTEXT_TIER_AUTO, QODER_CONTEXT_TIER_DEFAULT, QODER_CONTEXT_TIER_ENV,
    QODER_CONTEXT_TIER_HEADROOM, QODER_CONTEXT_TIER_MAX,
};

const UNIT_K: u64 = 1_000;
const UNIT_M: u64 = 1_000_000;

/// One model context tier.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextTier {
    pub name: String,
    pub token_count: u64,
    pub is_default: bool,
}

/// Result of [`resolve_context_tier`].
#[derive(Debug, Clone, PartialEq)]
pub struct TierChoice {
    pub tier: ContextTier,
    pub estimated_tokens: u64,
    pub reason: String,
}

/// `"200K" | "1M" | "204800" | 204800` → integer token count (0 when unparseable).
pub fn parse_tier_token_count(value: Option<&Json>) -> u64 {
    match value {
        Some(Json::Number(n)) => n.as_f64().filter(|f| *f > 0.0).map(|f| f.floor() as u64).unwrap_or(0),
        Some(Json::String(s)) => {
            let t = s.trim().to_uppercase();
            let (digits, unit) = match t.strip_suffix('K') {
                Some(d) => (d.trim(), UNIT_K),
                None => match t.strip_suffix('M') {
                    Some(d) => (d.trim(), UNIT_M),
                    None => (t.as_str(), 1),
                },
            };
            if digits.is_empty() {
                return 0;
            }
            // /^(\d+(?:\.\d+)?)$/
            let mut parts = digits.splitn(2, '.');
            let int = parts.next().unwrap_or("");
            let frac = parts.next();
            if int.is_empty() || !int.bytes().all(|b| b.is_ascii_digit()) {
                return 0;
            }
            if let Some(f) = frac {
                if f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) {
                    return 0;
                }
            }
            let n = digits.parse::<f64>().unwrap_or(0.0);
            if n.is_finite() && n > 0.0 {
                (n * unit as f64).floor() as u64
            } else {
                0
            }
        }
        _ => 0,
    }
}

fn tier_name(entry: &Json, token_count: u64) -> String {
    for key in ["name", "label", "display_name", "displayName"] {
        if let Some(s) = entry.get(key).and_then(Json::as_str) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    for key in ["key", "id"] {
        if let Some(s) = entry.get(key).and_then(Json::as_str) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    if token_count >= UNIT_M && token_count % UNIT_M == 0 {
        return format!("{}M", token_count / UNIT_M);
    }
    if token_count >= UNIT_K && token_count % UNIT_K == 0 {
        return format!("{}K", token_count / UNIT_K);
    }
    token_count.to_string()
}

/// `getQoderContextTiers` — sorted ascending, deduped by token count.
pub fn get_context_tiers(model_config: &Json) -> Vec<ContextTier> {
    let list = model_config
        .get("context_config")
        .or_else(|| model_config.get("contextConfig"))
        .and_then(Json::as_array);
    let Some(list) = list else { return Vec::new() };

    let mut order: Vec<u64> = Vec::new();
    let mut by_count: Vec<ContextTier> = Vec::new();

    for entry in list {
        if !entry.is_object() {
            continue;
        }
        let count = parse_tier_token_count(
            entry
                .get("tokenCount")
                .or_else(|| entry.get("token_count"))
                .or_else(|| entry.get("max_input_tokens"))
                .or_else(|| entry.get("maxInputTokens"))
                .or_else(|| entry.get("contextLength"))
                .or_else(|| entry.get("context_length")),
        );
        if count == 0 {
            continue;
        }
        let is_default = entry.get("isDefault") == Some(&Json::Bool(true))
            || entry.get("is_default") == Some(&Json::Bool(true))
            || entry.get("default") == Some(&Json::Bool(true));

        if let Some(pos) = order.iter().position(|c| *c == count) {
            by_count[pos].is_default = by_count[pos].is_default || is_default;
        } else {
            order.push(count);
            by_count.push(ContextTier { name: tier_name(entry, count), token_count: count, is_default });
        }
    }

    by_count.sort_by_key(|t| t.token_count);
    by_count
}

/// Rough prompt-size estimate in tokens. CJK characters count ~1 token each,
/// everything else ~4 chars/token — plain chars/4 underestimates
/// Chinese/Japanese by up to 4x, which is exactly when a tier decision matters.
pub fn estimate_prompt_tokens(system: &str, messages: &Json, tools: &Json) -> u64 {
    let text = json!({
        "system": if system.is_empty() { Json::String(String::new()) } else { Json::String(system.to_string()) },
        "messages": messages,
        "tools": tools,
    })
    .to_string();

    let mut cjk = 0u64;
    let mut chars = 0u64;
    for ch in text.chars() {
        let c = ch as u32;
        chars += 1;
        if (0x1100..=0x11ff).contains(&c)
            || (0x2e80..=0x9fff).contains(&c)
            || (0xac00..=0xd7af).contains(&c)
            || (0xf900..=0xfaff).contains(&c)
            || (0xff00..=0xffef).contains(&c)
        {
            cjk += 1;
        }
    }
    cjk + (chars - cjk).div_ceil(4)
}

fn normalize_mode(preference: Option<&str>) -> String {
    let p = preference.map(str::trim).unwrap_or("");
    if p.is_empty() {
        QODER_CONTEXT_TIER_AUTO.to_string()
    } else {
        p.to_string()
    }
}

fn find_named_tier<'a>(tiers: &'a [ContextTier], name: &str) -> Option<&'a ContextTier> {
    let wanted: String = name.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_uppercase();
    let as_count = parse_tier_token_count(Some(&Json::String(name.to_string())));
    tiers.iter().find(|t| {
        let tier_name: String = t.name.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_uppercase();
        tier_name == wanted || (as_count != 0 && t.token_count == as_count)
    })
}

/// `resolveQoderContextTier` — which tier this request should run under.
///
/// `None` means leave the payload exactly as before (no tiers, or the default
/// already fits).
pub fn resolve_context_tier(model_config: &Json, system: &str, messages: &Json, tools: &Json, preference: Option<&str>) -> Option<TierChoice> {
    let tiers = get_context_tiers(model_config);
    if tiers.is_empty() {
        return None;
    }

    let mode = normalize_mode(preference);
    let largest = tiers.last()?.clone();
    let default_tier = tiers.iter().find(|t| t.is_default).cloned().unwrap_or_else(|| tiers[0].clone());
    let estimated = estimate_prompt_tokens(system, messages, tools);
    let need = (estimated as f64 * (1.0 + QODER_CONTEXT_TIER_HEADROOM)).ceil() as u64;

    if mode.eq_ignore_ascii_case(QODER_CONTEXT_TIER_MAX) {
        return Some(TierChoice { tier: largest, estimated_tokens: estimated, reason: "forced:max".into() });
    }
    if mode.eq_ignore_ascii_case(QODER_CONTEXT_TIER_DEFAULT) {
        return Some(TierChoice { tier: default_tier, estimated_tokens: estimated, reason: "forced:default".into() });
    }
    if !mode.eq_ignore_ascii_case(QODER_CONTEXT_TIER_AUTO) {
        if let Some(named) = find_named_tier(&tiers, &mode) {
            return Some(TierChoice {
                tier: named.clone(),
                estimated_tokens: estimated,
                reason: format!("forced:{}", named.name),
            });
        }
        // Unknown tier name → fall through to auto rather than silently
        // breaking requests.
    }

    let current_max = parse_tier_token_count(
        model_config.get("max_input_tokens").or_else(|| model_config.get("maxInputTokens")),
    );
    let current_limit = if current_max > 0 { current_max } else { default_tier.token_count };
    if need <= current_limit {
        return None;
    }

    let fits = tiers.iter().find(|t| t.token_count >= need && t.token_count > current_limit);
    let chosen = fits.cloned().unwrap_or(largest);
    if chosen.token_count <= current_limit {
        // Nothing bigger to escalate to.
        return None;
    }
    Some(TierChoice {
        tier: chosen,
        estimated_tokens: estimated,
        reason: if fits.is_some() { "auto:fits".into() } else { "auto:largest".into() },
    })
}

/// Environment override for the tier mode (`QODER_CONTEXT_TIER`).
pub fn env_tier_preference() -> Option<String> {
    std::env::var(QODER_CONTEXT_TIER_ENV).ok().filter(|s| !s.trim().is_empty())
}

/// `applyQoderContextTier` — mirror the tier into the three payload slots.
pub fn apply_context_tier(payload: &mut Json, tier: &ContextTier) {
    if tier.token_count == 0 {
        return;
    }
    if let Some(params) = payload.get_mut("parameters").and_then(Json::as_object_mut) {
        params.insert("context_length".into(), json!(tier.token_count));
    } else if payload.get("parameters").is_none() {
        payload["parameters"] = json!({ "context_length": tier.token_count });
    }

    if !payload.get("chat_context").map(Json::is_object).unwrap_or(false) {
        payload["chat_context"] = Json::Object(Default::default());
    }
    if let Some(chat_context) = payload.get_mut("chat_context").and_then(Json::as_object_mut) {
        if !chat_context.get("extra").map(Json::is_object).unwrap_or(false) {
            chat_context.insert("extra".into(), Json::Object(Default::default()));
        }
        if let Some(extra) = chat_context.get_mut("extra").and_then(Json::as_object_mut) {
            if !extra.get("ideModelConfigOverride").map(Json::is_object).unwrap_or(false) {
                extra.insert("ideModelConfigOverride".into(), Json::Object(Default::default()));
            }
            if let Some(override_cfg) = extra.get_mut("ideModelConfigOverride").and_then(Json::as_object_mut) {
                override_cfg.insert("max_input_tokens".into(), json!(tier.token_count));
            }
        }
    }

    if payload.get("model_config").map(Json::is_object).unwrap_or(false) {
        if let Some(mc) = payload.get_mut("model_config").and_then(Json::as_object_mut) {
            mc.insert("max_input_tokens".into(), json!(tier.token_count));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_config() -> Json {
        json!({
            "key": "qmodel_38max",
            "max_input_tokens": 180000,
            "context_config": [
                { "name": "200K", "tokenCount": 200000, "isDefault": true },
                { "name": "400K", "tokenCount": 400000 },
                { "name": "1M", "tokenCount": 1000000 }
            ]
        })
    }

    #[test]
    fn parses_tier_token_counts() {
        assert_eq!(parse_tier_token_count(Some(&json!("200K"))), 200_000);
        assert_eq!(parse_tier_token_count(Some(&json!("1M"))), 1_000_000);
        assert_eq!(parse_tier_token_count(Some(&json!("204800"))), 204_800);
        assert_eq!(parse_tier_token_count(Some(&json!(204800))), 204_800);
        assert_eq!(parse_tier_token_count(Some(&json!("bogus"))), 0);
        assert_eq!(parse_tier_token_count(None), 0);
    }

    #[test]
    fn tiers_are_sorted_and_deduped() {
        let tiers = get_context_tiers(&model_config());
        assert_eq!(tiers.len(), 3);
        assert_eq!(tiers[0].name, "200K");
        assert!(tiers[0].is_default);
        assert_eq!(tiers[2].token_count, 1_000_000);

        let dupes = json!({ "context_config": [
            { "tokenCount": 200000 },
            { "tokenCount": 200000, "isDefault": true }
        ]});
        let tiers = get_context_tiers(&dupes);
        assert_eq!(tiers.len(), 1);
        assert!(tiers[0].is_default);
    }

    #[test]
    fn short_prompt_keeps_default_tier() {
        let prompt = json!([{ "role": "user", "content": "hello" }]);
        assert!(resolve_context_tier(&model_config(), "", &prompt, &json!([]), None).is_none());
    }

    #[test]
    fn long_prompt_escalates_to_fitting_tier() {
        // ~50k tokens of ASCII → need = 57.5k, which fits the current 180k
        // default. Push well past it to force escalation.
        let big = "x".repeat(900_000);
        let prompt = json!([{ "role": "user", "content": big }]);
        let choice = resolve_context_tier(&model_config(), "", &prompt, &json!([]), None).expect("escalation");
        assert_eq!(choice.reason, "auto:fits");
        assert_eq!(choice.tier.token_count, 400_000);
    }

    #[test]
    fn oversized_prompt_falls_back_to_largest_tier() {
        let big = "x".repeat(6_000_000);
        let prompt = json!([{ "role": "user", "content": big }]);
        let choice = resolve_context_tier(&model_config(), "", &prompt, &json!([]), None).expect("escalation");
        assert_eq!(choice.reason, "auto:largest");
        assert_eq!(choice.tier.token_count, 1_000_000);
    }

    #[test]
    fn forced_modes_win() {
        let prompt = json!([]);
        let max = resolve_context_tier(&model_config(), "", &prompt, &json!([]), Some("max")).unwrap();
        assert_eq!(max.tier.token_count, 1_000_000);
        let def = resolve_context_tier(&model_config(), "", &prompt, &json!([]), Some("default")).unwrap();
        assert_eq!(def.tier.token_count, 200_000);
        let named = resolve_context_tier(&model_config(), "", &prompt, &json!([]), Some("400k")).unwrap();
        assert_eq!(named.tier.token_count, 400_000);
        // Unknown name falls through to auto (fits → no choice).
        assert!(resolve_context_tier(&model_config(), "", &prompt, &json!([]), Some("nope")).is_none());
    }

    #[test]
    fn no_tiers_means_no_choice() {
        let cfg = json!({ "key": "auto", "max_input_tokens": 200000 });
        let big = "x".repeat(4_000_000);
        let prompt = json!([{ "role": "user", "content": big }]);
        assert!(resolve_context_tier(&cfg, "", &prompt, &json!([]), None).is_none());
    }

    #[test]
    fn apply_writes_all_three_slots() {
        let mut payload = json!({
            "parameters": { "max_tokens": 100 },
            "chat_context": { "extra": { "context": [] } },
            "model_config": { "key": "qmodel_38max" }
        });
        let tier = ContextTier { name: "1M".into(), token_count: 1_000_000, is_default: false };
        apply_context_tier(&mut payload, &tier);
        assert_eq!(payload["parameters"]["max_tokens"], json!(100));
        assert_eq!(payload["parameters"]["context_length"], json!(1_000_000));
        assert_eq!(payload["chat_context"]["extra"]["ideModelConfigOverride"]["max_input_tokens"], json!(1_000_000));
        assert_eq!(payload["chat_context"]["extra"]["context"], json!([]));
        assert_eq!(payload["model_config"]["max_input_tokens"], json!(1_000_000));
    }

    #[test]
    fn estimates_cjk_at_one_token_per_char() {
        let messages = json!([{ "role": "user", "content": "你好世界" }]);
        let est = estimate_prompt_tokens("", &messages, &json!([]));
        // 4 CJK chars + JSON scaffolding at 4 chars/token
        assert!(est >= 4);
    }
}
