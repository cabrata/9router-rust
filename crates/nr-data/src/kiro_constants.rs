use nr_core::Json;
use regex::Regex;
use std::sync::LazyLock;

const KIRO_AGENTIC_SUFFIX: &str = "-agentic";
const KIRO_THINKING_SUFFIX: &str = "-thinking";

/// Resolve the shared default profileArn for a given auth method.
pub fn resolve_default_profile_arn(auth_method: Option<&str>) -> &'static str {
    let is_social = match auth_method {
        Some(m) => m == "google" || m == "github",
        None => false,
    };
    if is_social {
        "arn:aws:codewhisperer:us-east-1:699475941385:profile/EHGA3GRVQMUK"
    } else {
        "arn:aws:codewhisperer:us-east-1:638616132270:profile/AAAACCCCXXXX"
    }
}

/// Fold an Anthropic model id onto the Kiro upstream alias.
pub fn normalize_kiro_upstream_model_id(model: Option<&str>) -> String {
    let model = match model {
        Some(m) => m,
        None => return String::new(),
    };

    if model.is_empty() {
        return String::new();
    }

    static SNAPSHOT_SUFFIX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"-\d{8}$").unwrap());

    let id = model.to_lowercase();
    let id = SNAPSHOT_SUFFIX.replace(&id, "").to_string();

    if id.contains("sonnet") {
        if id.contains("5.5") || id.contains("5-5") {
            return "claude-sonnet-5.5".to_string();
        }
        if id.contains("4.6") || id.contains("4-6") {
            return "claude-sonnet-4.6".to_string();
        }
        if id.contains("sonnet-5") || id.contains("sonnet.5") {
            return "claude-sonnet-5".to_string();
        }
        if id.contains("4.5") || id.contains("4-5") {
            return "claude-sonnet-4.5".to_string();
        }
        if id.contains("sonnet-4") || id.contains("sonnet.4") {
            return "claude-sonnet-4".to_string();
        }
        return "claude-sonnet-4.5".to_string();
    }
    if id.contains("fable") {
        return "claude-fable-5".to_string();
    }
    if id.contains("opus") {
        if id.contains("5.5") || id.contains("5-5") {
            return "claude-opus-5.5".to_string();
        }
        if id.contains("opus-5") || id.contains("opus.5") || id.contains("opus 5") {
            return "claude-opus-5".to_string();
        }
        if id.contains("4.8") || id.contains("4-8") {
            return "claude-opus-4.8".to_string();
        }
        if id.contains("4.7") || id.contains("4-7") {
            return "claude-opus-4.7".to_string();
        }
        if id.contains("4.5") || id.contains("4-5") {
            return "claude-opus-4.5".to_string();
        }
        return "claude-opus-4.6".to_string();
    }
    if id.contains("haiku") {
        if id.contains("5.5") || id.contains("5-5") {
            return "claude-haiku-5.5".to_string();
        }
        return "claude-haiku-4.5".to_string();
    }
    if id == "auto" {
        return "auto".to_string();
    }
    if id.contains("deepseek") {
        return "deepseek-3.2".to_string();
    }
    if id.contains("glm") {
        return "glm-5".to_string();
    }
    if id.contains("minimax") {
        if id.contains("2.5") || id.contains("2-5") {
            return "minimax-m2.5".to_string();
        }
        return "minimax-m2.1".to_string();
    }
    if id.contains("qwen") {
        return "qwen3-coder-next".to_string();
    }
    if id.contains("kimi") {
        return "kimi-k3".to_string();
    }
    if id.contains("gpt") {
        if id.contains("terra") {
            return "gpt-5.6-terra".to_string();
        }
        if id.contains("luna") {
            return "gpt-5.6-luna".to_string();
        }
        if id.contains("sol") || id.contains("5.6") || id.contains("5-6") {
            return "gpt-5.6-sol".to_string();
        }
        return model.to_string();
    }
    model.to_string()
}

/// Parse Claude version from model id
fn parse_claude_version(model: &str) -> Option<(u32, Option<u32>)> {
    let normalized = model.to_lowercase().replace('-', ".");
    static VERSION_REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?:^|[/.])claude(?:[/.][a-z]+)*[/.](\d+)(?:[/.](\d+))?(?:[/.]|$)").unwrap());

    VERSION_REGEX.captures(&normalized).map(|caps| {
        let major = caps.get(1).and_then(|m| m.as_str().parse::<u32>().ok()).unwrap_or(0);
        let minor = caps.get(2).and_then(|m| m.as_str().parse::<u32>().ok());
        (major, minor)
    })
}

/// Check if model lacks xhigh support (4.6 and earlier Claude)
fn kiro_model_lacks_xhigh(model: &str) -> bool {
    match parse_claude_version(model) {
        Some((major, Some(minor))) if major == 4 && minor <= 6 => true,
        Some((major, None)) if major < 4 => true,
        _ => false,
    }
}

/// Extract Kiro effort level from request body
pub fn extract_kiro_effort_level(body: Option<&Json>, model: Option<&str>) -> Option<String> {
    let body = body?;
    let effort = body
        .get("output_config")
        .and_then(|oc| oc.get("effort").and_then(Json::as_str))
        .or_else(|| body.get("reasoning_effort").and_then(Json::as_str))
        .or_else(|| {
            body.get("reasoning")
                .and_then(|r| r.as_object())
                .and_then(|r| r.get("effort").and_then(Json::as_str))
        });

    let effort = effort?;
    let normalized = effort.to_lowercase();

    if normalized == "none" || normalized == "off" || normalized == "disabled" {
        return None;
    }
    if normalized == "xhigh" {
        return Some(
            if let Some(m) = model {
                if kiro_model_lacks_xhigh(m) {
                    "high".to_string()
                } else {
                    "xhigh".to_string()
                }
            } else {
                "xhigh".to_string()
            },
        );
    }
    if normalized == "max" {
        return Some("max".to_string());
    }
    if ["low", "medium", "high"].contains(&normalized.as_str()) {
        return Some(normalized);
    }
    None
}

/// Detect whether an inbound request is asking for reasoning / thinking output.
pub fn is_thinking_enabled(
    body: Option<&Json>,
    headers: Option<&Json>,
    model: Option<&str>,
) -> bool {
    resolve_kiro_thinking_budget(body, headers, model).is_some()
}

/// Detect whether a model id refers to a 9router synthetic agentic variant.
pub fn is_agentic_model(model: Option<&str>) -> bool {
    match model {
        Some(m) => m.ends_with(KIRO_AGENTIC_SUFFIX),
        None => false,
    }
}

/// Strip the `-agentic` suffix from a model id.
pub fn strip_agentic_suffix(model: &str) -> String {
    if is_agentic_model(Some(model)) {
        model[..model.len() - KIRO_AGENTIC_SUFFIX.len()].to_string()
    } else {
        model.to_string()
    }
}

/// Detect whether a model id is a 9router synthetic thinking variant.
pub fn is_thinking_model(model: Option<&str>) -> bool {
    match model {
        Some(m) => m.ends_with(KIRO_THINKING_SUFFIX),
        None => false,
    }
}

/// Strip the `-thinking` suffix from a model id.
pub fn strip_thinking_suffix(model: &str) -> String {
    if is_thinking_model(Some(model)) {
        model[..model.len() - KIRO_THINKING_SUFFIX.len()].to_string()
    } else {
        model.to_string()
    }
}

/// Resolve a 9router model id to the real upstream Kiro model id, plus flags.
pub fn resolve_kiro_model(model: &str) -> serde_json::Value {
    let mut upstream = model.to_string();
    let mut agentic = false;
    let mut thinking = false;

    if is_agentic_model(Some(&upstream)) {
        agentic = true;
        upstream = strip_agentic_suffix(&upstream);
    }
    if is_thinking_model(Some(&upstream)) {
        thinking = true;
        upstream = strip_thinking_suffix(&upstream);
    }

    serde_json::json!({
        "upstream": normalize_kiro_upstream_model_id(Some(&upstream)),
        "agentic": agentic,
        "thinking": thinking
    })
}

/// Parse a trailing "model(level)" / "model(budget)" suffix.
/// Mirrors thinkingUnified.js parseSuffix(): returns (cleanModel, override).
/// TODO(dedupe): use nr_translator::concerns::thinking_unified::parse_suffix once
/// a shared crate owns it (nr-data cannot depend on nr-translator).
fn parse_suffix(model: &str) -> (String, Option<Json>) {
    static SUFFIX_REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^(.*)\(([^()]+)\)\s*$").unwrap());
    let Some(caps) = SUFFIX_REGEX.captures(model) else {
        return (model.to_string(), None);
    };
    let clean_model = caps.get(1).map(|m| m.as_str().trim().to_string()).unwrap_or_default();
    let raw = caps.get(2).map(|m| m.as_str().trim().to_lowercase()).unwrap_or_default();

    let override_cfg = match raw.as_str() {
        "none" | "off" => serde_json::json!({"mode": "none"}),
        "auto" => serde_json::json!({"mode": "auto"}),
        "ultra" => serde_json::json!({"mode": "level", "level": raw}),
        _ if raw.chars().all(|c| c.is_ascii_digit()) && !raw.is_empty() => {
            serde_json::json!({"mode": "budget", "budget": raw.parse::<f64>().unwrap_or(0.0)})
        }
        // Known effort levels (LEVEL_TO_BUDGET keys).
        _ if matches!(
            raw.as_str(),
            "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
        ) => serde_json::json!({"mode": "level", "level": raw}),
        _ => return (clean_model, None),
    };
    (clean_model, Some(override_cfg))
}

/// Resolve a Kiro model after consuming the generic model(level) suffix.
pub fn resolve_kiro_model_intent(model: &str) -> serde_json::Value {
    let (clean_model, thinking_override) = parse_suffix(model);
    let mut result = resolve_kiro_model(&clean_model);
    result["thinkingOverride"] = thinking_override.unwrap_or(Json::Null);
    result
}

/// Apply a parsed model(level) override without mutating the caller's body.
pub fn apply_kiro_thinking_override(
    body: &Json,
    thinking_override: Option<&Json>,
) -> Json {
    let thinking_override = match thinking_override {
        Some(o) => o,
        None => return body.clone(),
    };

    let mut next = body.clone();
    if let serde_json::Value::Object(ref mut obj) = next {
        if let Some(mode) = thinking_override.get("mode").and_then(Json::as_str) {
            if mode == "budget" {
                obj.remove("output_config");
                obj.remove("reasoning_effort");
                obj.remove("reasoning");
                if let Some(budget) = thinking_override.get("budget") {
                    obj.insert(
                        "thinking".to_string(),
                        serde_json::json!({"type": "enabled", "budget_tokens": budget}),
                    );
                }
            } else {
                let effort = if mode == "level" {
                    thinking_override
                        .get("level")
                        .and_then(Json::as_str)
                        .unwrap_or(mode)
                } else {
                    mode
                };
                let output_config = obj
                    .get("output_config")
                    .and_then(|oc| oc.as_object())
                    .cloned()
                    .unwrap_or_default();
                let mut new_config = output_config;
                new_config.insert("effort".to_string(), Json::String(effort.to_string()));
                obj.insert("output_config".to_string(), Json::Object(new_config));
            }
        }
    }
    next
}

/// Extract the unified thinking intent from a request body (mixed wire shapes).
/// Mirrors thinkingUnified.js extractThinking(): { mode, budget?, level? } or None.
/// TODO(dedupe): use nr_translator::concerns::thinking_unified::extract_thinking
/// once a shared crate owns it (nr-data cannot depend on nr-translator).
fn extract_thinking(body: Option<&Json>) -> Option<Json> {
    let body = body?;

    // Claude output_config.effort (explicit) - priority over adaptive thinking
    if let Some(oc) = body.get("output_config").and_then(|o| o.get("effort")).and_then(Json::as_str) {
        if !oc.is_empty() {
            let e = oc.to_lowercase();
            return Some(match e.as_str() {
                "none" | "off" => serde_json::json!({"mode": "none"}),
                "auto" => serde_json::json!({"mode": "auto"}),
                _ => serde_json::json!({"mode": "level", "level": e}),
            });
        }
    }

    // OpenAI chat / Responses shape - effort first (zai sends both thinking object
    // and reasoning.effort)
    let effort = body
        .get("reasoning_effort")
        .and_then(Json::as_str)
        .or_else(|| {
            body.get("reasoning")
                .and_then(|r| r.as_object())
                .and_then(|r| r.get("effort"))
                .and_then(Json::as_str)
        });
    if let Some(effort) = effort.filter(|e| !e.is_empty()) {
        let e = effort.to_lowercase();
        return Some(match e.as_str() {
            "none" | "off" => serde_json::json!({"mode": "none"}),
            "auto" => serde_json::json!({"mode": "auto"}),
            _ => serde_json::json!({"mode": "level", "level": e}),
        });
    }

    // Claude shape
    if let Some(t) = body.get("thinking").filter(|t| t.is_object()) {
        match t.get("type").and_then(Json::as_str) {
            Some("disabled") => return Some(serde_json::json!({"mode": "none"})),
            Some("adaptive") | Some("enabled") => {
                let budget = t.get("budget_tokens").and_then(Json::as_f64).unwrap_or(0.0);
                if budget > 0.0 {
                    return Some(serde_json::json!({"mode": "budget", "budget": budget}));
                }
                return Some(serde_json::json!({"mode": "auto"}));
            }
            _ => {}
        }
    }

    // Gemini shape (top-level, generationConfig, or request envelope)
    if let Some(tc) = body
        .get("thinkingConfig")
        .or_else(|| body.get("generationConfig").and_then(|g| g.get("thinkingConfig")))
        .or_else(|| {
            body.get("request")
                .and_then(|r| r.get("generationConfig"))
                .and_then(|g| g.get("thinkingConfig"))
        })
    {
        if let Some(level) = tc.get("thinkingLevel").and_then(Json::as_str) {
            return Some(serde_json::json!({"mode": "level", "level": level.to_lowercase()}));
        }
        if let Some(tb) = tc.get("thinkingBudget").and_then(Json::as_f64) {
            if tb == 0.0 {
                return Some(serde_json::json!({"mode": "none"}));
            }
            if tb < 0.0 {
                return Some(serde_json::json!({"mode": "auto"}));
            }
            return Some(serde_json::json!({"mode": "budget", "budget": tb}));
        }
    }

    None
}

/// effort level -> budget_tokens (thinking.js LEVEL_TO_BUDGET).
/// TODO(dedupe): use nr_translator::concerns::thinking::effort_to_budget once
/// a shared crate owns it.
fn effort_to_budget(effort: &str) -> Option<u32> {
    Some(match effort.to_lowercase().as_str() {
        "none" => 0,
        "minimal" => 512,
        "low" => 1024,
        "medium" => 8192,
        "high" => 24576,
        "xhigh" => 32768,
        "max" => 128000,
        _ => return None,
    })
}

/// Read a header case-insensitively from a plain JSON header map.
fn pick_header<'a>(headers: &'a Json, name: &str) -> Option<&'a str> {
    let lower = name.to_lowercase();
    headers
        .as_object()?
        .iter()
        .find(|(key, _)| key.to_lowercase() == lower)
        .and_then(|(_, value)| value.as_str())
}

/// True when the body already carries an enabled/interleaved thinking_mode tag.
fn contains_thinking_mode_tag(body: &Json) -> bool {
    fn tag_in_text(text: &str) -> bool {
        text.contains("<thinking_mode>")
            && (text.contains("<thinking_mode>enabled</thinking_mode>")
                || text.contains("<thinking_mode>interleaved</thinking_mode>"))
    }

    let messages = body.get("messages").and_then(Json::as_array);
    for msg in messages.into_iter().flatten() {
        let role = msg.get("role").and_then(Json::as_str);
        if role != Some("system") && role != Some("user") {
            continue;
        }
        match msg.get("content") {
            Some(Json::String(text)) if tag_in_text(text) => return true,
            Some(Json::Array(parts)) => {
                for part in parts {
                    if let Some(text) = part.get("text").and_then(Json::as_str) {
                        if tag_in_text(text) {
                            return true;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    body.get("system")
        .and_then(Json::as_str)
        .map(tag_in_text)
        .unwrap_or(false)
}

/// Resolve the Kiro thinking budget requested by a client.
/// Returns None when thinking is disabled (JS resolveKiroThinkingBudget).
pub fn resolve_kiro_thinking_budget(
    body: Option<&Json>,
    headers: Option<&Json>,
    model: Option<&str>,
) -> Option<u32> {
    const DEFAULT_BUDGET: u32 = 16000;

    if let Some(cfg) = extract_thinking(body) {
        match cfg.get("mode").and_then(Json::as_str) {
            Some("none") => return None,
            Some("level") => {
                let level = cfg.get("level").and_then(Json::as_str).unwrap_or("");
                if level == "disabled" {
                    return None;
                }
                return Some(effort_to_budget(level).unwrap_or(DEFAULT_BUDGET));
            }
            Some("budget") => {
                let budget = cfg.get("budget").and_then(Json::as_f64).unwrap_or(0.0);
                return Some(budget.max(0.0) as u32);
            }
            _ => return Some(DEFAULT_BUDGET),
        }
    }

    if let Some(headers) = headers {
        if let Some(beta) = pick_header(headers, "anthropic-beta") {
            if beta.to_lowercase().contains("interleaved-thinking") {
                return Some(DEFAULT_BUDGET);
            }
        }
    }

    if let Some(body) = body {
        if contains_thinking_mode_tag(body) {
            return Some(DEFAULT_BUDGET);
        }
    }

    if let Some(m) = model.filter(|m| !m.is_empty()) {
        let m = m.to_lowercase();
        if m.contains("thinking") || m.contains("-reason") {
            return Some(DEFAULT_BUDGET);
        }
    }

    None
}

/// Build the magic system-prompt prefix that turns Kiro reasoning on.
pub fn build_thinking_system_prefix(budget: Option<u32>) -> String {
    let safe_budget = budget
        .unwrap_or(16000)
        .max(1)
        .min(32000);
    format!(
        "<thinking_mode>enabled</thinking_mode>\n<max_thinking_length>{}</max_thinking_length>",
        safe_budget
    )
}

/// Resolve the effort path for a model (output_config or reasoning)
pub fn resolve_kiro_effort_path(model: Option<&str>) -> Option<String> {
    let model = model?;
    let normalized = model.to_lowercase().replace('-', ".");

    if normalized.contains("gpt") && normalized.contains("5.6") {
        return Some("reasoning".to_string());
    }

    if !normalized.contains("claude") {
        return None;
    }

    let (major, minor) = parse_claude_version(&normalized)?;
    if major < 4 || (major == 4 && (minor.is_none() || minor == Some(5))) {
        return None;
    }
    if major == 4 && minor.is_some() && minor != Some(5) {
        // Check for date suffix indicating old model
        if let Some(minor_val) = minor {
            if minor_val >= 1000 {
                return None; // Date suffix, old model
            }
        }
    }
    Some("output_config".to_string())
}

/// Check if model supports additional model request fields
pub fn supports_kiro_additional_model_request_fields(model: Option<&str>) -> bool {
    resolve_kiro_effort_path(model).is_some()
}

/// Check if model uses Kiro native GPT effort
pub fn uses_kiro_native_gpt_effort(body: Option<&Json>, model: Option<&str>) -> bool {
    if resolve_kiro_effort_path(model).as_deref() != Some("reasoning") {
        return false;
    }
    extract_kiro_gpt_effort_level(body).is_some()
}

/// Build Kiro additional model request fields for a model
pub fn build_kiro_additional_model_request_fields_for_model(
    body: Option<&Json>,
    model: Option<&str>,
) -> Option<Json> {
    let effort_path = resolve_kiro_effort_path(model)?;
    build_kiro_additional_model_request_fields(body, Some(effort_path.as_str()), model)
}

/// Build Kiro additional model request fields
pub fn build_kiro_additional_model_request_fields(
    body: Option<&Json>,
    effort_path: Option<&str>,
    model: Option<&str>,
) -> Option<Json> {
    let effort = if effort_path == Some("reasoning") {
        extract_kiro_gpt_effort_level(body)?
    } else {
        extract_kiro_effort_level(body, model)?
    };

    if effort_path == Some("reasoning") {
        Some(serde_json::json!({"reasoning": {"effort": effort}}))
    } else {
        Some(serde_json::json!({
            "thinking": {"type": "adaptive", "display": "summarized"},
            "output_config": {"effort": effort}
        }))
    }
}

/// Extract Kiro GPT effort level from request body
fn extract_kiro_gpt_effort_level(body: Option<&Json>) -> Option<String> {
    let body = body?;
    let effort = body
        .get("output_config")
        .and_then(|oc| oc.get("effort").and_then(Json::as_str))
        .or_else(|| body.get("reasoning_effort").and_then(Json::as_str))
        .or_else(|| {
            body.get("reasoning")
                .and_then(|r| r.as_object())
                .and_then(|r| r.get("effort").and_then(Json::as_str))
        });

    let effort = effort?;
    let normalized = effort.to_lowercase();

    if normalized == "max" {
        return Some("xhigh".to_string());
    }
    if ["low", "medium", "high", "xhigh"].contains(&normalized.as_str()) {
        return Some(normalized);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_kiro_upstream_model_id() {
        assert_eq!(
            normalize_kiro_upstream_model_id(Some("claude-sonnet-4-5-20250929")),
            "claude-sonnet-4.5"
        );
        assert_eq!(
            normalize_kiro_upstream_model_id(Some("claude-opus-5")),
            "claude-opus-5"
        );
    }

    #[test]
    fn test_strip_agentic_suffix() {
        assert_eq!(
            strip_agentic_suffix("claude-sonnet-4.5-agentic"),
            "claude-sonnet-4.5"
        );
        assert_eq!(
            strip_agentic_suffix("claude-sonnet-4.5"),
            "claude-sonnet-4.5"
        );
    }

    #[test]
    fn test_resolve_kiro_model() {
        let result = resolve_kiro_model("claude-sonnet-4.5-thinking-agentic");
        assert_eq!(result.get("thinking").and_then(Json::as_bool), Some(true));
        assert_eq!(result.get("agentic").and_then(Json::as_bool), Some(true));
    }

    #[test]
    fn test_resolve_default_profile_arn() {
        assert_eq!(
            resolve_default_profile_arn(Some("google")),
            "arn:aws:codewhisperer:us-east-1:699475941385:profile/EHGA3GRVQMUK"
        );
        assert_eq!(
            resolve_default_profile_arn(Some("github")),
            "arn:aws:codewhisperer:us-east-1:699475941385:profile/EHGA3GRVQMUK"
        );
        assert_eq!(
            resolve_default_profile_arn(Some("builder-id")),
            "arn:aws:codewhisperer:us-east-1:638616132270:profile/AAAACCCCXXXX"
        );
    }

    #[test]
    fn test_is_agentic_model() {
        assert!(is_agentic_model(Some("claude-sonnet-4.5-agentic")));
        assert!(!is_agentic_model(Some("claude-sonnet-4.5")));
    }

    #[test]
    fn test_is_thinking_model() {
        assert!(is_thinking_model(Some("claude-sonnet-4.5-thinking")));
        assert!(!is_thinking_model(Some("claude-sonnet-4.5")));
    }

    #[test]
    fn test_build_thinking_system_prefix() {
        let prefix = build_thinking_system_prefix(Some(16000));
        assert!(prefix.contains("<thinking_mode>enabled</thinking_mode>"));
        assert!(prefix.contains("<max_thinking_length>16000</max_thinking_length>"));
    }

    #[test]
    fn test_resolve_kiro_effort_path() {
        assert_eq!(
            resolve_kiro_effort_path(Some("claude-opus-4.6")),
            Some("output_config".to_string())
        );
        assert_eq!(resolve_kiro_effort_path(Some("claude-opus-4.5")), None);
    }
}
