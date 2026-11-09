//! Unified thinking normalization: extract client intent → apply provider-native format.
//! Port of open-sse/translator/concerns/thinkingUnified.js.
//!
//! Config-driven: thinking format/limits come from capabilities + registry transport,
//! never hardcoded per-model here. See .docs/thinking/plan.md MATRIX VI-A.

use nr_core::Json;
use serde_json::json;

use crate::concerns::thinking::{budget_to_level, effort_to_budget, effort_to_thinking_level};

/// Map a target wire-format to its native thinking format (when capability has none).
fn format_to_native(target_format: &str) -> &'static str {
    match target_format {
        "openai" | "openai-responses" | "openai-response" | "codex" => "openai",
        "claude" => "claude-budget",
        "gemini" | "gemini-cli" | "vertex" | "antigravity" => "gemini-budget",
        "kiro" => "kiro",
        "commandcode" => "commandcode",
        _ => "openai",
    }
}

/// Strip a trailing thinking suffix "model(value)" → "model" (no-op when absent).
pub fn strip_thinking_suffix(model: &str) -> &str {
    // JS: /^(.*)\([^()]+\)\s*$/
    let trimmed = model.trim_end();
    if !trimmed.ends_with(')') {
        return model;
    }
    let open = match trimmed.rfind('(') {
        Some(i) => i,
        None => return model,
    };
    let inner = &trimmed[open + 1..trimmed.len() - 1];
    if inner.is_empty() || inner.contains('(') || inner.contains(')') {
        return model;
    }
    trimmed[..open].trim_end()
}

/// Parse model-name suffix "model(value)" → (cleanModel, override).
/// value: level name (high) | number (8192) | auto | none. None override when absent.
pub fn parse_suffix(model: &str) -> (String, Option<Json>) {
    let trimmed = model.trim_end();
    if !trimmed.ends_with(')') {
        return (model.to_string(), None);
    }
    let open = match trimmed.rfind('(') {
        Some(i) => i,
        None => return (model.to_string(), None),
    };
    let inner = &trimmed[open + 1..trimmed.len() - 1];
    if inner.is_empty() || inner.contains('(') || inner.contains(')') {
        return (model.to_string(), None);
    }
    let clean_model = trimmed[..open].trim().to_string();
    let raw = inner.trim().to_lowercase();

    if raw == "none" || raw == "off" {
        return (clean_model, Some(json!({"mode": "none"})));
    }
    if raw == "auto" {
        return (clean_model, Some(json!({"mode": "auto"})));
    }
    if raw == "ultra" {
        return (clean_model, Some(json!({"mode": "level", "level": raw})));
    }
    if !raw.is_empty() && raw.chars().all(|c| c.is_ascii_digit()) {
        if let Ok(n) = raw.parse::<i64>() {
            return (clean_model, Some(json!({"mode": "budget", "budget": n})));
        }
    }
    if effort_to_budget(&raw).is_some() {
        return (clean_model, Some(json!({"mode": "level", "level": raw})));
    }
    (clean_model, None)
}

fn effort_intent(e: &str) -> Json {
    let l = e.to_lowercase();
    if l == "none" || l == "off" {
        json!({"mode": "none"})
    } else if l == "auto" {
        json!({"mode": "auto"})
    } else {
        json!({"mode": "level", "level": l})
    }
}

/// Extract unified thinking intent from a request body (post-translation, mixed shapes).
/// Returns { mode, budget?, level? } or null when no thinking intent present.
pub fn extract_thinking(body: &Json) -> Option<Json> {
    if !body.is_object() {
        return None;
    }

    // Claude output_config.effort (explicit) — priority over adaptive thinking
    if let Some(oc) = body.get("output_config").and_then(|v| v.get("effort")).and_then(|v| v.as_str()) {
        if !oc.is_empty() {
            return Some(effort_intent(oc));
        }
    }

    // OpenAI chat / Responses shape — effort first (zai sends both thinking object and reasoning.effort)
    let effort = body
        .get("reasoning_effort")
        .and_then(|v| v.as_str())
        .or_else(|| {
            body.get("reasoning")
                .filter(|r| r.is_object())
                .and_then(|r| r.get("effort"))
                .and_then(|v| v.as_str())
        });
    if let Some(effort) = effort.filter(|e| !e.is_empty()) {
        return Some(effort_intent(effort));
    }

    // Claude shape
    if let Some(t) = body.get("thinking").filter(|v| v.is_object()) {
        match t.get("type").and_then(|v| v.as_str()) {
            Some("disabled") => return Some(json!({"mode": "none"})),
            Some("adaptive") | Some("enabled") => {
                let budget = t.get("budget_tokens").and_then(json_number);
                if let Some(b) = budget.filter(|b| *b > 0.0) {
                    return Some(json!({"mode": "budget", "budget": b as i64}));
                }
                return Some(json!({"mode": "auto"}));
            }
            _ => {}
        }
    }

    // Gemini shape (top-level, generationConfig, or request envelope)
    let tc = body.get("thinkingConfig").filter(|v| v.is_object()).or_else(|| {
        body.get("generationConfig")
            .and_then(|g| g.get("thinkingConfig"))
            .filter(|v| v.is_object())
    }).or_else(|| {
        body.get("request")
            .and_then(|r| r.get("generationConfig"))
            .and_then(|g| g.get("thinkingConfig"))
            .filter(|v| v.is_object())
    });
    if let Some(tc) = tc {
        if let Some(level) = tc.get("thinkingLevel").and_then(|v| v.as_str()) {
            return Some(json!({"mode": "level", "level": level.to_lowercase()}));
        }
        if let Some(tb) = tc.get("thinkingBudget").and_then(json_number) {
            if tb == 0.0 {
                return Some(json!({"mode": "none"}));
            }
            if tb < 0.0 {
                return Some(json!({"mode": "auto"}));
            }
            return Some(json!({"mode": "budget", "budget": tb as i64}));
        }
    }

    // Qwen shape
    if body.get("enable_thinking").and_then(|v| v.as_bool()) == Some(false) {
        return Some(json!({"mode": "none"}));
    }
    if body.get("enable_thinking").and_then(|v| v.as_bool()) == Some(true) {
        let tb = body.get("thinking_budget").and_then(json_number);
        if let Some(b) = tb.filter(|b| *b > 0.0) {
            return Some(json!({"mode": "budget", "budget": b as i64}));
        }
        return Some(json!({"mode": "auto"}));
    }

    None
}

fn json_number(v: &Json) -> Option<f64> {
    match v {
        Json::Number(n) => n.as_f64(),
        Json::String(s) => s.trim().parse::<f64>().ok(),
        Json::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

fn openai_thinking_display(body: &Json) -> Option<&'static str> {
    // Responses API: reasoning.summary is the explicit request for reasoning text.
    if let Some(reasoning) = body.get("reasoning").filter(|v| v.is_object()) {
        if let Some(summary) = reasoning.get("summary").and_then(|v| v.as_str()) {
            return if !summary.is_empty() && summary != "none" {
                Some("summarized")
            } else {
                None
            };
        }
        return None;
    }
    // Chat Completions has no summary knob. A client setting reasoning_effort is
    // asking for reasoning, and reasoning_content is how it would receive it.
    if body.get("reasoning_effort").and_then(|v| v.as_str()).is_some() {
        return Some("summarized");
    }
    None
}

/// Capture thinking intent from a body before format translation strips it.
/// Besides the effort, records whether an OpenAI-shaped client wants the thinking
/// text itself: Claude returns it only with thinking.display "summarized", a field
/// OpenAI has no equivalent for, so the intent cannot survive translation on its own.
pub fn capture_thinking(body: &Json) -> Option<Json> {
    let cfg = extract_thinking(body)?;
    if cfg.get("mode").and_then(|v| v.as_str()) == Some("none") {
        return Some(cfg);
    }
    match openai_thinking_display(body) {
        Some(display) => {
            let mut out = cfg;
            if let Some(obj) = out.as_object_mut() {
                obj.insert("display".to_string(), json!(display));
            }
            Some(out)
        }
        None => Some(cfg),
    }
}

fn is_native_only_format(fmt: &str) -> bool {
    matches!(fmt, "gemini-level" | "gemini-budget" | "claude-budget" | "claude-adaptive" | "kiro")
}

/// Resolve the thinking wire format for the target.
fn resolve_format(target_format: &str, model: &str, provider: Option<&str>) -> String {
    if target_format == "commandcode" {
        return "commandcode".to_string();
    }
    // A multi-endpoint provider can declare, per transports[] entry, the thinking
    // shape that endpoint accepts. It wins over the model's thinkingFormat.
    if let Some(p) = provider {
        if let Some(transports) = nr_data::provider(p).and_then(|v| v.get("transports")).and_then(|v| v.as_array()) {
            if let Some(t) = transports.iter().find(|t| t.get("format").and_then(|f| f.as_str()) == Some(target_format)) {
                if let Some(fmt) = t.get("thinkingFormat").and_then(|v| v.as_str()) {
                    return fmt.to_string();
                }
            }
        }
        if let Some(fmt) = nr_data::provider(p).and_then(|v| v.get("thinkingFormat")).and_then(|v| v.as_str()) {
            return fmt.to_string();
        }
    }
    let caps = nr_data::capabilities::get_capabilities_for_model(provider, Some(model));
    let is_openai_wire = target_format == "openai" || target_format == "openai-responses";
    if let Some(fmt) = caps.get("thinkingFormat").and_then(|v| v.as_str()) {
        if !(is_openai_wire && is_native_only_format(fmt)) {
            return fmt.to_string();
        }
    }
    format_to_native(target_format).to_string()
}

fn cfg_mode(cfg: &Json) -> &str {
    cfg.get("mode").and_then(|v| v.as_str()).unwrap_or("")
}

fn to_budget(cfg: &Json, range: Option<&Json>) -> Option<i64> {
    let mut budget = match cfg_mode(cfg) {
        "budget" => cfg.get("budget").and_then(json_number),
        "level" => cfg.get("level").and_then(|v| v.as_str()).and_then(effort_to_budget).map(|b| b as f64),
        "auto" => return Some(-1),
        _ => None,
    }?;
    if let Some(range) = range {
        if let Some(min) = range.get("min").and_then(json_number) {
            if budget < min {
                budget = min;
            }
        }
        if let Some(max) = range.get("max").and_then(json_number) {
            if budget > max {
                budget = max;
            }
        }
    }
    Some(budget as i64)
}

fn to_level(cfg: &Json) -> Option<String> {
    match cfg_mode(cfg) {
        "level" => cfg.get("level").and_then(|v| v.as_str()).map(|s| s.to_string()),
        "budget" => {
            let b = cfg.get("budget").and_then(json_number).unwrap_or(0.0) as i64;
            Some(budget_to_level(b).unwrap_or("medium").to_string())
        }
        "auto" => Some("auto".to_string()),
        _ => None,
    }
}

fn normalize_openai_level(level: &str, supported: Option<&Vec<String>>) -> String {
    if level != "max" && level != "ultra" {
        return level.to_string();
    }
    let has = |l: &str| supported.is_some_and(|s| s.iter().any(|x| x == l));
    if has(level) {
        return level.to_string();
    }
    if level == "ultra" && has("max") {
        return "max".to_string();
    }
    "xhigh".to_string()
}

fn to_gemini_thinking_level(cfg: &Json) -> String {
    let raw = if cfg_mode(cfg) == "auto" {
        "high".to_string()
    } else {
        to_level(cfg).unwrap_or_else(|| "high".to_string())
    };
    effort_to_thinking_level(&raw)
}

fn to_kimi_reasoning_effort(cfg: &Json) -> Option<&'static str> {
    let level = to_level(cfg)?;
    match level.as_str() {
        "auto" => Some("high"),
        "minimal" => Some("low"),
        "xhigh" => Some("max"),
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        "max" => Some("max"),
        _ => None,
    }
}

fn gemini_level_output_floor(level: &str) -> i64 {
    match level {
        "minimal" => 4096,
        "low" => 8192,
        "medium" => 16384,
        _ => 65535,
    }
}

fn gemini_budget_output_floor(budget: i64) -> i64 {
    if budget == -1 {
        return 32768;
    }
    if budget <= 1024 {
        8192
    } else if budget <= 8192 {
        16384
    } else if budget <= 24576 {
        32768
    } else {
        65535
    }
}

/// Mutable `generationConfig` (envelope-aware). The returned reference cannot be
/// held across the body borrow, so callers use `set_gemini_thinking` instead.
fn gemini_generation_config_mut(body: &mut Json) -> &mut Json {
    if body.get("request").map(|r| r.is_object()).unwrap_or(false) {
        let request = body.get_mut("request").unwrap();
        if !request.get("generationConfig").map(|g| g.is_object()).unwrap_or(false) {
            request["generationConfig"] = json!({});
        }
        return &mut request["generationConfig"];
    }
    if !body.get("generationConfig").map(|g| g.is_object()).unwrap_or(false) {
        body["generationConfig"] = json!({});
    }
    &mut body["generationConfig"]
}

fn set_gemini_thinking(body: &mut Json, tc: Json) {
    gemini_generation_config_mut(body)["thinkingConfig"] = tc;
}

fn ensure_gemini_output_floor(body: &mut Json, floor: i64, caps: &Json) {
    let cap = caps.get("maxOutput").and_then(json_number);
    let target = match cap {
        Some(c) => (floor as f64).min(c) as i64,
        None => floor,
    };
    let gc = gemini_generation_config_mut(body);
    let current = gc.get("maxOutputTokens").and_then(json_number);
    if current.is_none_or(|c| c < target as f64) {
        gc["maxOutputTokens"] = json!(target);
    }
}

fn logical(params: &Json, key: &str) -> bool {
    params.get(key).map(|v| nr_core::json::truthy(v)).unwrap_or(false)
}

/// Strip every known thinking field from a body.
fn strip_all(body: &mut Json) {
    if let Some(obj) = body.as_object_mut() {
        for key in ["thinking", "reasoning_effort", "reasoning", "thinkingConfig", "enable_thinking", "thinking_budget", "output_config"] {
            obj.remove(key);
        }
    }
    if let Some(gen) = body.get_mut("generationConfig").and_then(|g| g.as_object_mut()) {
        gen.remove("thinkingConfig");
    }
    if let Some(request) = body.get_mut("request").and_then(|r| r.as_object_mut()) {
        if let Some(gen) = request.get_mut("generationConfig").and_then(|g| g.as_object_mut()) {
            gen.remove("thinkingConfig");
        }
    }
    if let Some(params) = body.get_mut("params").and_then(|p| p.as_object_mut()) {
        params.remove("reasoning_effort");
        params.remove("thinking");
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_format(
    fmt: &str,
    body: &mut Json,
    cfg: &Json,
    caps: &Json,
    supported_levels: Option<&Vec<String>>,
    display: Option<&str>,
    model: &str,
    budget_passthrough: bool,
) {
    let none = cfg_mode(cfg) == "none";
    let can_disable = caps.get("thinkingCanDisable").and_then(|v| v.as_bool()) != Some(false);
    // Model cannot disable thinking → clamp "none" to minimal effort instead.
    let eff: Json = if none && !can_disable {
        json!({"mode": "level", "level": "minimal"})
    } else {
        cfg.clone()
    };

    match fmt {
        "openai" => {
            if none && can_disable {
                set_key(body, "reasoning_effort", json!("none"));
                return;
            }
            if let Some(level) = to_level(&eff) {
                set_key(body, "reasoning_effort", json!(normalize_openai_level(&level, supported_levels)));
            }
        }
        "openai-responses" => {
            // The Responses API nests effort: reasoning:{effort,summary}.
            if none && can_disable {
                if let Some(obj) = body.as_object_mut() {
                    obj.remove("reasoning");
                }
                return;
            }
            if let Some(level) = to_level(&eff) {
                let current = body
                    .get("reasoning")
                    .filter(|r| r.is_object() && !r.is_array())
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let mut reasoning = current;
                reasoning["effort"] = json!(normalize_openai_level(&level, supported_levels));
                if !nr_core::json::truthy(&reasoning["summary"]) {
                    reasoning["summary"] = json!("auto");
                }
                set_key(body, "reasoning", reasoning);
            }
            if let Some(obj) = body.as_object_mut() {
                obj.remove("reasoning_effort");
            }
        }
        "claude-adaptive" => {
            if none && can_disable {
                set_key(body, "thinking", json!({"type": "disabled"}));
                return;
            }
            if let Some(level) = to_level(&eff) {
                // Models that can disable thinking need the explicit adaptive switch.
                if can_disable {
                    let mut t = json!({"type": "adaptive"});
                    if let Some(d) = display {
                        t["display"] = json!(d);
                    }
                    set_key(body, "thinking", t);
                } else if let Some(d) = display.filter(|_| level != "minimal" && model.to_lowercase().contains("claude")) {
                    set_key(body, "thinking", json!({"type": "adaptive", "display": d}));
                } else if let Some(obj) = body.as_object_mut() {
                    obj.remove("thinking");
                }
                let xhigh_ok = supported_levels.is_some_and(|s| s.iter().any(|x| x == "xhigh"));
                let effort = if level == "auto" {
                    "high"
                } else if level == "minimal" {
                    "low"
                } else if level == "xhigh" && !xhigh_ok {
                    "high"
                } else {
                    level.as_str()
                };
                set_key(body, "output_config", json!({"effort": effort}));
            }
        }
        "claude-budget" => {
            if none && can_disable {
                set_key(body, "thinking", json!({"type": "disabled"}));
                return;
            }
            let budget = to_budget(&eff, caps.get("thinkingRange"));
            let mut t = if budget == Some(-1) {
                json!({"type": "enabled"})
            } else {
                json!({"type": "enabled", "budget_tokens": budget.unwrap_or(8192)})
            };
            if let Some(d) = display {
                t["display"] = json!(d);
            }
            set_key(body, "thinking", t);
        }
        "gemini-level" => {
            let mut level = if none {
                "minimal".to_string()
            } else {
                to_gemini_thinking_level(&eff)
            };
            // Models without a "minimal" tier (Gemini 4) reject it with 400.
            if let Some(levels) = supported_levels {
                if !levels.is_empty() && !levels.iter().any(|l| *l == level) {
                    if let Some(lowest) = levels.iter().find(|l| l.as_str() != "none") {
                        level = lowest.clone();
                    }
                }
            }
            let floor = gemini_level_output_floor(&level);
            set_gemini_thinking(body, json!({
                "thinkingLevel": level,
                "includeThoughts": level != "minimal"
            }));
            ensure_gemini_output_floor(body, floor, caps);
        }
        "gemini-budget" => {
            if none && can_disable {
                set_gemini_thinking(body, json!({"thinkingBudget": 0, "includeThoughts": false}));
                return;
            }
            let budget = to_budget(&eff, caps.get("thinkingRange")).unwrap_or(-1);
            let floor = gemini_budget_output_floor(budget);
            set_gemini_thinking(body, json!({"thinkingBudget": budget, "includeThoughts": true}));
            ensure_gemini_output_floor(body, floor, caps);
        }
        "zai" => {
            if none && can_disable {
                set_key(body, "enable_thinking", json!(false));
                if let Some(obj) = body.as_object_mut() {
                    obj.remove("thinking");
                }
                return;
            }
            set_key(body, "thinking", json!({"type": "enabled"}));
            if logical(caps, "thinkingEffortSupported") {
                let zai_level = to_level(&eff);
                let effort = match zai_level.as_deref() {
                    Some("low") | Some("minimal") => "low",
                    Some("high") | Some("medium") => "high",
                    Some(_) => "max",
                    None => "max",
                };
                set_key(body, "reasoning_effort", json!(effort));
            }
        }
        "qwen" => {
            if none && can_disable {
                set_key(body, "enable_thinking", json!(false));
                return;
            }
            set_key(body, "enable_thinking", json!(true));
            if let Some(budget) = to_budget(&eff, caps.get("thinkingRange")) {
                if budget > 0 {
                    set_key(body, "thinking_budget", json!(budget));
                }
            }
        }
        "deepseek" => {
            if none && can_disable {
                set_key(body, "thinking", json!({"type": "disabled"}));
                return;
            }
            let budget = cfg.get("budget").and_then(json_number).unwrap_or(0.0);
            if budget_passthrough && cfg_mode(cfg) == "budget" && budget > 0.0 {
                set_key(body, "thinking", json!({"type": "enabled", "budget_tokens": budget as i64}));
            } else {
                set_key(body, "thinking", json!({"type": "enabled"}));
            }
            let level = to_level(&eff).unwrap_or_default();
            let want = if level == "xhigh" || level == "max" {
                "max"
            } else if (level == "low" || level == "minimal")
                && supported_levels.is_some_and(|s| s.iter().any(|l| l == "low"))
            {
                "low"
            } else {
                "high"
            };
            let want = if want == "max"
                && supported_levels.is_some_and(|s| !s.iter().any(|l| l == "max"))
            {
                "high"
            } else {
                want
            };
            set_key(body, "reasoning_effort", json!(want));
        }
        "kimi" => {
            if none && can_disable {
                set_key(body, "thinking", json!({"type": "disabled"}));
                return;
            }
            if let Some(effort) = to_kimi_reasoning_effort(&eff) {
                set_key(body, "reasoning_effort", json!(effort));
            }
        }
        "minimax" => {
            // M3 adaptive; M2.x cannot disable (handled via canDisable clamp).
            let t = if none && can_disable { "disabled" } else { "adaptive" };
            set_key(body, "thinking", json!({"type": t}));
        }
        "hunyuan" => {
            if none && can_disable {
                set_key(body, "thinking", json!({"type": "disabled"}));
                return;
            }
            let budget = to_budget(&eff, caps.get("thinkingRange"));
            let t = if budget == Some(-1) {
                json!({"type": "enabled"})
            } else {
                json!({"type": "enabled", "budget_tokens": budget.unwrap_or(8192)})
            };
            set_key(body, "thinking", t);
        }
        "step" => {
            if none && can_disable {
                return;
            }
            if let Some(level) = to_level(&eff) {
                let effort = if level == "xhigh" || level == "max" { "high" } else { level.as_str() };
                set_key(body, "reasoning_effort", json!(effort));
            }
        }
        "nous" => {
            if none && can_disable {
                return;
            }
            let level = if cfg_mode(&eff) == "auto" {
                Some("medium".to_string())
            } else {
                to_level(&eff)
            };
            let mut r = json!({"enabled": true});
            if let Some(l) = level {
                r["effort"] = json!(l);
            }
            set_key(body, "reasoning", r);
        }
        "tokenrouter" => {
            // TokenRouter rejects "none"/"auto" with a 400; pass levels through.
            if none || cfg_mode(&eff) == "auto" {
                return;
            }
            if let Some(level) = to_level(&eff) {
                set_key(body, "reasoning_effort", json!(level));
            }
        }
        "kiro" => {
            // Kiro thinking handled via system-tag injection in openai-to-kiro; no body field.
        }
        "commandcode" => {
            if !body.get("params").map(|p| p.is_object()).unwrap_or(false) {
                body["params"] = json!({});
            }
            if none && can_disable {
                if let Some(params) = body.get_mut("params").and_then(|p| p.as_object_mut()) {
                    params.remove("reasoning_effort");
                }
                return;
            }
            let raw_level = to_level(&eff);
            let level = match raw_level.as_deref() {
                Some("auto") => Some("high".to_string()),
                Some("minimal") => Some("low".to_string()),
                other => other.map(|s| s.to_string()),
            };
            let valid = ["off", "low", "medium", "high", "xhigh", "max"];
            if let Some(level) = level.filter(|l| valid.contains(&l.as_str())) {
                set_key(body, "params/reasoning_effort", json!(level));
            }
        }
        _ => {}
    }
}

/// Insert `value` at `path` ("key", or "parent/child"), creating parents.
fn set_key(body: &mut Json, path: &str, value: Json) {
    match path.split_once('/') {
        Some((parent, key)) => {
            if !body.get(parent).map(|v| v.is_object()).unwrap_or(false) {
                body[parent] = json!({});
            }
            body[parent][key] = value;
        }
        None => {
            body[path] = value;
        }
    }
}

/// Apply thinking config to the target format.
/// Mutates and returns body. No-op when model has no reasoning capability.
/// `intent` is a pre-captured config (from `capture_thinking` on the original body);
/// falls back to extracting from the current body when omitted.
pub fn apply_thinking(
    target_format: &str,
    model: &str,
    body: &mut Json,
    provider: Option<&str>,
    thinking_intent: Option<&Json>,
) {
    if !body.is_object() {
        return;
    }

    let (clean_model, override_cfg) = parse_suffix(model);
    let cfg = match override_cfg.or_else(|| thinking_intent.cloned()) {
        Some(c) => c,
        None => match extract_thinking(body) {
            Some(c) => c,
            None => return,
        },
    };
    let caps = nr_data::capabilities::get_capabilities_for_model(provider, Some(&clean_model));

    // Model cannot reason → strip any stray thinking fields.
    if !logical(&caps, "reasoning") {
        strip_all(body);
        return;
    }

    let fmt = resolve_format(target_format, &clean_model, provider);
    let supported_levels = nr_data::thinking_levels::get_thinking_levels(provider, Some(&clean_model));
    // Anthropic's `display` (summarized | omitted) decides whether thinking text
    // comes back at all; keep what the client asked for instead of resetting it.
    let display: Option<String> = body
        .get("thinking")
        .and_then(|t| t.get("display"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            cfg.get("display")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        });
    let is_openai_compatible = provider
        .map(|p| p.starts_with("openai-compatible-"))
        .unwrap_or(false);
    strip_all(body);
    apply_format(
        &fmt,
        body,
        &cfg,
        &caps,
        supported_levels.as_ref(),
        display.as_deref(),
        &clean_model,
        is_openai_compatible,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_thinking_openai() {
        let body = json!({"reasoning_effort": "high"});
        let result = extract_thinking(&body).unwrap();
        assert_eq!(result["level"], "high");
    }

    #[test]
    fn test_extract_thinking_claude() {
        let body = json!({"thinking": {"type": "enabled", "budget_tokens": 8192}});
        let result = extract_thinking(&body).unwrap();
        assert_eq!(result["budget"], 8192);
    }

    #[test]
    fn test_extract_thinking_gemini_budget_zero() {
        let body = json!({"generationConfig": {"thinkingConfig": {"thinkingBudget": 0}}});
        assert_eq!(extract_thinking(&body).unwrap()["mode"], "none");
    }

    #[test]
    fn test_extract_thinking_session_envelope() {
        let body = json!({"request": {"generationConfig": {"thinkingConfig": {"thinkingLevel": "HIGH"}}}});
        let got = extract_thinking(&body).unwrap();
        assert_eq!(got["mode"], "level");
        assert_eq!(got["level"], "high");
    }

    #[test]
    fn test_capture_thinking_marks_display() {
        let body = json!({"reasoning_effort": "low"});
        let got = capture_thinking(&body).unwrap();
        assert_eq!(got["display"], "summarized");
    }

    #[test]
    fn test_capture_thinking_summary_none() {
        let body = json!({"reasoning": {"effort": "low", "summary": "none"}});
        let got = capture_thinking(&body).unwrap();
        assert!(got.get("display").is_none());
    }

    #[test]
    fn test_strip_thinking_suffix() {
        assert_eq!(strip_thinking_suffix("gpt-5(high)"), "gpt-5");
        assert_eq!(strip_thinking_suffix("gpt-5"), "gpt-5");
        // JS regex is greedy: only the outermost suffix group is stripped.
        assert_eq!(strip_thinking_suffix("claude(a)(b)"), "claude(a)");
    }

    #[test]
    fn test_parse_suffix() {
        assert_eq!(parse_suffix("m(8192)").1.unwrap()["budget"], 8192);
        assert_eq!(parse_suffix("m(auto)").1.unwrap()["mode"], "auto");
        assert_eq!(parse_suffix("m").1, None);
    }

    fn caps_reasoning() -> Json {
        // getCapabilitiesForModel is data-driven; force a reasoning-capable model so the
        // per-format application runs even when the provider table lacks the exact id.
        json!({"reasoning": true, "thinkingCanDisable": true})
    }

    #[test]
    fn test_apply_openai_level() {
        let mut body = json!({});
        apply_format("openai", &mut body, &json!({"mode":"level","level":"high"}), &caps_reasoning(), None, None, "gpt-5.1", false);
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn test_apply_openai_none() {
        let mut body = json!({});
        apply_format("openai", &mut body, &json!({"mode":"none"}), &caps_reasoning(), None, None, "gpt-5.1", false);
        assert_eq!(body["reasoning_effort"], "none");
    }

    #[test]
    fn test_apply_claude_budget() {
        let mut body = json!({});
        apply_format("claude-budget", &mut body, &json!({"mode":"budget","budget":4096}), &caps_reasoning(), None, None, "claude", false);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 4096);
    }

    #[test]
    fn test_apply_claude_adaptive_xhigh_clamp() {
        let mut body = json!({});
        apply_format(
            "claude-adaptive",
            &mut body,
            &json!({"mode":"level","level":"xhigh"}),
            &caps_reasoning(),
            Some(&vec!["none".into(), "low".into(), "medium".into(), "high".into()]),
            None,
            "claude",
            false,
        );
        assert_eq!(body["output_config"]["effort"], "high");
    }

    #[test]
    fn test_apply_responses_nests_effort() {
        let mut body = json!({});
        apply_format("openai-responses", &mut body, &json!({"mode":"level","level":"low"}), &caps_reasoning(), None, None, "gpt-5.1", false);
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["reasoning"]["effort"], "low");
        assert_eq!(body["reasoning"]["summary"], "auto");
    }

    #[test]
    fn test_apply_gemini_budget_nested_envelope() {
        let mut body = json!({"request": {"generationConfig": {}}});
        apply_format("gemini-budget", &mut body, &json!({"mode":"budget","budget":8192}), &caps_reasoning(), None, None, "gemini-3-pro", false);
        assert_eq!(body["request"]["generationConfig"]["thinkingConfig"]["thinkingBudget"], 8192);
    }

    #[test]
    fn test_apply_gemini_budget_no_nested_envelope() {
        let mut body = json!({});
        apply_format("gemini-budget", &mut body, &json!({"mode":"auto"}), &caps_reasoning(), None, None, "gemini-3-pro", false);
        assert_eq!(body["generationConfig"]["thinkingConfig"]["thinkingBudget"], -1);
        assert_eq!(body["generationConfig"]["thinkingConfig"]["includeThoughts"], true);
        // maxOutputTokens floor for auto (-1) is 32768.
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 32768);
    }

    #[test]
    fn test_apply_gemini_level_no_minimal() {
        let mut body = json!({});
        apply_format(
            "gemini-level",
            &mut body,
            &json!({"mode":"level","level":"minimal"}),
            &caps_reasoning(),
            Some(&vec!["low".into(), "medium".into(), "high".into()]),
            None,
            "gemini-4",
            false,
        );
        assert_eq!(body["generationConfig"]["thinkingConfig"]["thinkingLevel"], "low");
    }

    #[test]
    fn test_apply_zai_binary_off() {
        let mut body = json!({"thinking": {"type": "enabled"}});
        apply_format("zai", &mut body, &json!({"mode":"none"}), &caps_reasoning(), None, None, "glm-5", false);
        assert_eq!(body["enable_thinking"], false);
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn test_apply_commandcode_params() {
        let mut body = json!({});
        apply_format("commandcode", &mut body, &json!({"mode":"auto"}), &caps_reasoning(), None, None, "cc", false);
        assert_eq!(body["params"]["reasoning_effort"], "high");
    }

    #[test]
    fn test_apply_qwen_budget() {
        let mut body = json!({});
        apply_format("qwen", &mut body, &json!({"mode":"budget","budget":1024}), &caps_reasoning(), None, None, "qwen", false);
        assert_eq!(body["enable_thinking"], true);
        assert_eq!(body["thinking_budget"], 1024);
    }

    #[test]
    fn test_apply_none_disabled() {
        let mut body = json!({"thinking": {"type": "enabled"}});
        apply_format("claude-budget", &mut body, &json!({"mode":"none"}), &caps_reasoning(), None, None, "claude", false);
        assert_eq!(body["thinking"]["type"], "disabled");
    }

    #[test]
    fn test_apply_none_without_disable_clamps_to_minimal() {
        let mut body = json!({});
        apply_format(
            "openai",
            &mut body,
            &json!({"mode":"none"}),
            &json!({"reasoning": true, "thinkingCanDisable": false}),
            None,
            None,
            "gpt-5.1",
            false,
        );
        assert_eq!(body["reasoning_effort"], "minimal");
    }

    #[test]
    fn test_apply_thinking_strips_when_no_reasoning_cap() {
        let mut body = json!({"reasoning_effort": "high", "thinking": {"type": "enabled"}});
        apply_thinking("openai", "text-embedding-3-small", &mut body, Some("openai"), None);
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn test_apply_thinking_no_intent_noop() {
        let mut body = json!({"messages": [{"role": "user", "content": "hi"}]});
        apply_thinking("openai", "gpt-5.1", &mut body, Some("openai"), None);
        assert!(body.get("reasoning_effort").is_none());
    }
}
