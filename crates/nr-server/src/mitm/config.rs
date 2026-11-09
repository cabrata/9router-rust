//! Intercepted domains, URL patterns and model extraction.
//! Port of src/mitm/config.js.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use nr_core::json;

/// Hosts the MITM server answers TLS for. Any other SNI passes through untouched.
pub const TARGET_HOSTS: &[&str] = &[
    "daily-cloudcode-pa.googleapis.com",
    "cloudcode-pa.googleapis.com",
    "api.individual.githubcopilot.com",
    "q.us-east-1.amazonaws.com",
    "codewhisperer.us-east-1.amazonaws.com",
    "runtime.us-east-1.kiro.dev",
    "api2.cursor.sh",
];

/// URL substrings that mark a chat turn worth intercepting, per tool.
/// Kiro's legacy path form; Kiro IDE 1.0.228+ posts to `/` with an
/// `x-amz-target` header instead (see `is_chat_request`).
pub fn url_patterns(tool: &str) -> &'static [&'static str] {
    match tool {
        "antigravity" => &[":generateContent", ":streamGenerateContent"],
        "copilot" => &["/chat/completions", "/v1/messages", "/responses"],
        // Legacy path form. Kiro IDE 1.0.228+ posts to `/` with x-amz-target instead —
        // see is_chat_request() for the header-based match.
        "kiro" => &["/generateAssistantResponse"],
        "cursor" => &["/BidiAppend", "/RunSSE", "/RunPoll", "/Run"],
        _ => &[],
    }
}

/// Whether this request is a chat turn we should intercept (vs passthrough).
pub fn is_chat_request(tool: &str, path_and_query: &str, headers: &http::HeaderMap) -> bool {
    if url_patterns(tool).iter().any(|p| path_and_query.contains(p)) {
        return true;
    }
    // Kiro Runtime moved GenerateAssistantResponse from path `/generateAssistantResponse`
    // to `POST /` + `x-amz-target: KiroRuntimeService.GenerateAssistantResponse`
    // (verified via live mitmproxy capture of Kiro IDE 1.0.228).
    if tool == "kiro" {
        let target = headers
            .get("x-amz-target")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        return target.contains("GenerateAssistantResponse");
    }
    false
}

/// Map a tool's host → tool id. `None` for hosts we don't intercept.
pub fn get_tool_for_host(host: &str) -> Option<&'static str> {
    let h = host.split(':').next().unwrap_or("");
    match h {
        "api.individual.githubcopilot.com" => Some("copilot"),
        "daily-cloudcode-pa.googleapis.com" | "cloudcode-pa.googleapis.com" => Some("antigravity"),
        "q.us-east-1.amazonaws.com"
        | "codewhisperer.us-east-1.amazonaws.com"
        | "runtime.us-east-1.kiro.dev" => Some("kiro"),
        "api2.cursor.sh" => Some("cursor"),
        _ => None,
    }
}

/// Synonym map: rawModel from request → canonical alias key in mitmAlias DB.
/// Reads from the data dump when present; `None` just means no synonyms for
/// this tool (the alias DB is the source of truth at request time).
fn model_synonyms(tool: &str) -> Option<&'static Value> {
    nr_data::export("src/shared/constants/mitmModelSynonyms", "default")
        .or_else(|| nr_data::export("src/shared/constants/mitmModelSynonyms", "module.exports"))
        .and_then(|v| v.get(tool))
}

/// Regex fallbacks catching renamed model variants (e.g. deprecated pro IDs).
struct ModelPattern {
    regex: Regex,
    alias: &'static str,
}

/// Order matters: first match wins, so broader patterns come after specific ones.
static MODEL_PATTERNS_ANTIGRAVITY: LazyLock<Vec<ModelPattern>> = LazyLock::new(|| {
    let mut v: Vec<ModelPattern> = Vec::new();
    for (pat, alias) in [
        // flash patterns first: a request naming "flash" in any form is a flash slot.
        (r#"flash.*extra.*low|extra.*low.*flash|flash.*low|low.*flash"#, "gemini-3.8-flash-low"),
        (r#"flash.*medium|medium.*flash"#, "gemini-3.8-flash-medium"),
        (r#"flash.*agent|agent.*flash|flash"#, "gemini-3.8-flash-high"),
        (r#"pro.*low|low.*pro"#, "gemini-3.1-pro-low"),
        (r#"gemini.*pro|pro.*gemini"#, "gemini-pro-agent"),
        (r#"opus.*5[.-]5.*high|5[.-]5.*opus.*high"#, "claude-opus-5-5-high"),
        (r#"opus.*5[.-]5.*medium|5[.-]5.*opus.*medium"#, "claude-opus-5-5-medium"),
        (r#"opus.*5[.-]5.*low|5[.-]5.*opus.*low"#, "claude-opus-5-5-low"),
        (r#"sonnet.*5[.-]5.*high|5[.-]5.*sonnet.*high"#, "claude-sonnet-5-5-high"),
        (r#"sonnet.*5[.-]5.*medium|5[.-]5.*sonnet.*medium"#, "claude-sonnet-5-5-medium"),
        (r#"sonnet.*5[.-]5.*low|5[.-]5.*sonnet.*low"#, "claude-sonnet-5-5-low"),
        (r#"opus.*5[.-]5|5[.-]5.*opus"#, "claude-opus-5-5"),
        (r#"sonnet.*5[.-]5|5[.-]5.*sonnet"#, "claude-sonnet-5-5"),
        (r#"opus"#, "claude-opus-4-6-thinking"),
        (r#"sonnet|claude"#, "claude-sonnet-4-6"),
        (r#"gpt.*oss|oss"#, "gpt-oss-120b-medium"),
    ] {
        if let Ok(r) = Regex::new(&format!("(?i){pat}")) {
            v.push(ModelPattern { regex: r, alias });
        }
    }
    v
});

fn model_patterns(tool: &str) -> &'static [ModelPattern] {
    match tool {
        "antigravity" => &MODEL_PATTERNS_ANTIGRAVITY,
        _ => &[],
    }
}

/// Pattern-based alias lookup for a normalized model id, if any pattern matches.
pub fn pattern_alias(tool: &str, model: &str) -> Option<&'static str> {
    model_patterns(tool)
        .iter()
        .find(|p| p.regex.is_match(model))
        .map(|p| p.alias)
}

/// Models that must NEVER be re-routed — always passthrough to the real upstream, even when
/// the tool's other models are mapped. Antigravity's tab-autocomplete (`tab_jump_flash_lite_preview`,
/// `tab_flash_lite_preview`, requestType tab/tab_jump) is latency-critical inline completion; routing
/// it through 9Router to an external chat model makes typing laggy and burns provider quota per
/// keystroke. Without this guard the broad `flash` pattern in MODEL_PATTERNS hijacks them onto the
/// flash-agent slot. Verified via MITM dump capture of streamGenerateContent (see AI_JOURNAL).
static MODEL_NO_MAP_ANTIGRAVITY: LazyLock<Vec<Regex>> =
    LazyLock::new(|| vec![Regex::new(r"(?i)^tab[_-]").unwrap()]);

fn model_no_map(tool: &str) -> &'static [Regex] {
    match tool {
        "antigravity" => &MODEL_NO_MAP_ANTIGRAVITY,
        _ => &[],
    }
}

pub fn model_is_no_map(tool: &str, model: &str) -> bool {
    model_no_map(tool).iter().any(|r| r.is_match(model))
}

/// URL substrings whose request/response should NOT be dumped to file (telemetry, polling, empty)
const LOG_BLACKLIST_URL_PARTS: &[&str] = &[
    "recordCodeAssistMetrics",
    "recordTrajectoryAnalytics",
    "fetchAdminControls",
    "listExperiments",
    "fetchUserInfo",
];

pub fn is_blacklisted(url: &str) -> bool {
    LOG_BLACKLIST_URL_PARTS.iter().any(|p| url.contains(p))
}

/// Heuristic: is this buffer binary (protobuf / EventStream) rather than JSON text?
fn is_binary_data(buffer: &[u8]) -> bool {
    if buffer.is_empty() {
        return false;
    }
    let sample = &buffer[..buffer.len().min(100)];
    let mut non_printable = 0usize;
    for &b in sample {
        if b < 0x20 && !matches!(b, 0x09 | 0x0A | 0x0D) {
            non_printable += 1;
        }
        if b > 0x7E {
            non_printable += 1;
        }
    }
    (non_printable as f64 / sample.len() as f64) > 0.3
}

/// Extract model from URL path (Gemini), body (OpenAI/Anthropic), or Kiro conversationState.
pub fn extract_model(url: &str, body: &[u8]) -> Option<String> {
    // /models/<id> or /models/<id>:generateContent
    let url_model = url
        .split_once("/models/")
        .and_then(|(_, rest)| {
            let end = rest.find(['/', ':']).unwrap_or(rest.len());
            let id = &rest[..end];
            if id.is_empty() { None } else { Some(id.to_string()) }
        });

    if is_binary_data(body) {
        return url_model;
    }

    let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    if !parsed.is_null() {
        // Kiro CodeWhisperer body carries the model deep in conversationState.
        if let Some(m) = json::get_path(
            &parsed,
            &["conversationState", "currentMessage", "userInputMessage", "modelId"],
        )
        .and_then(|v| v.as_str())
        {
            return Some(m.to_string());
        }

        let raw = url_model
            .clone()
            .or_else(|| parsed.get("model").and_then(|v| v.as_str()).map(str::to_string));
        if let Some(model) = raw {
            let clean = model.strip_prefix("models/").unwrap_or(&model);
            // Gemini "tiered" model ids encode the thinking level at request time, not in the
            // model name: resolve `gemini-3.x-flash-tiered` + thinkingLevel → a concrete slot.
            if clean == "gemini-3.6-flash-tiered"
                || clean == "gemini-3.7-flash-tiered"
                || clean == "gemini-3.8-flash-tiered"
            {
                let ver = if clean.contains("3.8") {
                    "3.8"
                } else if clean.contains("3.7") {
                    "3.7"
                } else {
                    "3.6"
                };
                let raw_level = json::get_path(
                    &parsed,
                    &["request", "generationConfig", "thinkingConfig", "thinkingLevel"],
                )
                .or_else(|| {
                    json::get_path(
                        &parsed,
                        &["generationConfig", "thinkingConfig", "thinkingLevel"],
                    )
                })
                .and_then(|v| v.as_str())
                .unwrap_or("");
                let level = match raw_level.to_ascii_lowercase().as_str() {
                    "high" | "medium" | "low" => raw_level.to_ascii_lowercase(),
                    _ => "medium".to_string(),
                };
                return Some(format!("gemini-{ver}-flash-{level}"));
            }
            return Some(model);
        }
    }
    url_model
}

/// Normalize a raw model id via the synonym table. Public so handlers can reuse it.
pub fn normalize_model(tool: &str, model: &str) -> String {
    let normalized = model.strip_prefix("models/").unwrap_or(model);
    if let Some(alias) = model_synonyms(tool)
        .and_then(|syn| syn.get(normalized))
        .and_then(|v| v.as_str())
    {
        return alias.to_string();
    }
    normalized.to_string()
}
