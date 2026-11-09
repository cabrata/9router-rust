//! Port of open-sse/executors/antigravity.js.
//!
//! AntigravityExecutor routes requests through Google's Gemini (via Anthropic's
//! Antigravity integration). Key features:
//!
//! - IDE request ID generation and tracking (agent/org/project/dev/sessionId)
//! - Request normalization (sanitize function names, strip unsupported fields)
//! - Tool cloaking to prevent anti-ban (client tools get _ide suffix)
//! - Quota routing across multiple models
//! - Retry logic with transient error detection
//! - Image model detection and configuration
//! - Session ID resolution
//! - Thought signature generation for extended thinking

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use indexmap::IndexMap;
use nr_common::session_manager::resolve_session_id;
use nr_core::http::HeaderList;
use nr_core::{Credentials, ExecRequest, ExecResult, Executor, Json, JsonMap, Result};
use nr_exec_base::base::{base_execute, BaseHooks};
use serde_json::json;
use sha2::{Digest, Sha256};

/// Antigravity tool cloaking suffix
const AG_TOOL_SUFFIX: &str = "_ide";

/// Fields that Antigravity rejects
const ANTIGRAVITY_REQUEST_BLACKLIST: &[&str] = &[
    "output_config",
    "thinking",
    "reasoning_effort",
    "reasoning",
    "enable_thinking",
    "thinking_budget",
    "thinkingConfig",
];

/// Maximum output tokens Antigravity allows
const MAX_ANTIGRAVITY_OUTPUT_TOKENS: i64 = 64000;

/// Maximum retry wait time
const MAX_RETRY_AFTER_MS: u64 = 10000;

/// Maximum transient retry backoff
const ANTIGRAVITY_TRANSIENT_RETRY_MAX_MS: u64 = 15000;

/// IDE request ID format regex
const ANTIGRAVITY_IDE_REQUEST_ID_RE: &str = r"^agent/[^/]+/\d+/[^/]+/\d+$";

/// Antigravity decoy tools (unused by IDE, injected for anti-detection)
const AG_DECOY_TOOL_NAMES: &[&str] = &[
    "browser_subagent",
    "command_status",
    "find_by_name",
    "generate_image",
    "grep_search",
    "list_dir",
    "list_resources",
    "mcp_sequential-thinking_sequentialthinking",
    "multi_replace_file_content",
    "notify_user",
    "read_resource",
    "read_terminal",
    "read_url_content",
    "replace_file_content",
    "run_command",
    "search_web",
    "send_command_input",
    "task_boundary",
    "view_content_chunk",
    "view_file",
    "write_to_file",
];

/// Generate a UUID v5 from a seed string
fn uuid_from_seed(seed: &str) -> String {
    let digest = Sha256::digest(seed.as_bytes());
    let mut bytes = digest[0..16].to_vec();

    // Set UUID v5 version and variant
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    let hex = hex::encode(&bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Build IDE request ID in format: agent/{conversationId}/{timestamp}/{trajectoryId}/{step}
fn build_ide_request_id(
    body: &Json,
    credentials: &nr_core::Credentials,
    model: &str,
    request_type: &str,
) -> String {
    // Check if body already has a valid IDE request ID
    if let Some(Json::String(req_id)) = body.get("requestId") {
        if regex::Regex::new(ANTIGRAVITY_IDE_REQUEST_ID_RE)
            .ok()
            .and_then(|re| Some(re.is_match(req_id)))
            .unwrap_or(false)
        {
            return req_id.clone();
        }
    }

    // Resolve session ID
    let session_id = if let Some(Json::Object(request_obj)) = body.get("request") {
        if let Some(Json::String(sid)) = request_obj.get("sessionId") {
            sid.clone()
        } else {
            resolve_session_id(
                None,
                body,
                credentials.connection_id(),
                None,
                "antigravity",
            )
        }
    } else {
        resolve_session_id(
            None,
            body,
            credentials.connection_id(),
            None,
            "antigravity",
        )
    };

    // Generate deterministic conversation and trajectory IDs
    let conversation_id = uuid_from_seed(&format!("antigravity:conversation:{}", session_id));
    let trajectory_id = uuid_from_seed(&format!(
        "antigravity:trajectory:{}:{}:{}",
        session_id, model, request_type
    ));

    // Count contents for step calculation
    let content_count = if let Some(Json::Object(request_obj)) = body.get("request") {
        if let Some(Json::Array(contents)) = request_obj.get("contents") {
            contents.len()
        } else {
            1
        }
    } else {
        1
    };

    let step = std::cmp::max(1, content_count * 2 - 1);

    format!(
        "agent/{}/{}/{}/{}",
        conversation_id,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        trajectory_id,
        step
    )
}

/// Parse retry time from error message (e.g. "Your quota will reset after 2h7m23s")
fn parse_retry_from_error_message(message: &str) -> Option<u64> {
    let re = regex::Regex::new(r"reset after (\d+h)?(\d+m)?(\d+s)?")
        .ok()?;
    let caps = re.captures(message)?;

    let mut total_ms: u64 = 0;
    if let Some(h_match) = caps.get(1) {
        if let Ok(h) = h_match.as_str().trim_end_matches('h').parse::<u64>() {
            total_ms += h * 3600 * 1000;
        }
    }
    if let Some(m_match) = caps.get(2) {
        if let Ok(m) = m_match.as_str().trim_end_matches('m').parse::<u64>() {
            total_ms += m * 60 * 1000;
        }
    }
    if let Some(s_match) = caps.get(3) {
        if let Ok(s) = s_match.as_str().trim_end_matches('s').parse::<u64>() {
            total_ms += s * 1000;
        }
    }

    if total_ms > 0 {
        Some(total_ms)
    } else {
        None
    }
}

/// Strip blacklisted fields from an object
fn strip_blacklisted(obj: &mut JsonMap) {
    for key in ANTIGRAVITY_REQUEST_BLACKLIST {
        obj.remove(*key);
    }
}

/// Prompt fragments competing-client branding that Antigravity flags with a
/// detail-free 429 RESOURCE_EXHAUSTED.
/// Port of `ANTIGRAVITY_PROMPT_REWRITES` (open-sse/config/appConstants.js).
const ANTIGRAVITY_PROMPT_REWRITES: &[(&str, &str)] = &[
    ("You are a Claude agent, built on Anthropic's Claude Agent SDK.", ""),
    ("opencode", "antigravity"),
];

fn rewrite_system_instruction(request: &mut Json) {
    let Some(parts) = request
        .get_mut("systemInstruction")
        .and_then(|s| s.get_mut("parts"))
        .and_then(Json::as_array_mut)
    else {
        return;
    };
    for part in parts {
        let Some(text) = part.get("text").and_then(Json::as_str) else { continue };
        let mut rewritten = text.to_string();
        for (from, to) in ANTIGRAVITY_PROMPT_REWRITES {
            rewritten = rewritten.replace(from, to);
        }
        // Hermes branding and the Anthropic billing header are regex-only.
        if let Ok(re) = regex::Regex::new(
            r"(?i)You are Hermes(?: Agent)?(?:,\s*(?:an intelligent AI assistant|an AI assistant|an AI agent))?(?:,?\s*(?:built|created)\s+by\s+Nous Research)?\.",
        ) {
            rewritten = re.replace_all(&rewritten, "You are an AI assistant.").into_owned();
        }
        if let Ok(re) = regex::Regex::new(r"(?im)^x-anthropic-billing-header:[^\n]*(?:\r?\n)*") {
            rewritten = re.replace_all(&rewritten, "").into_owned();
        }
        part["text"] = Json::String(rewritten);
    }
}

/// `AntigravityExecutor.transformRequest` — build the `v1internal` envelope.
pub fn transform_request(
    exec: &AntigravityExecutor,
    model: &str,
    body: &Json,
    stream: bool,
    credentials: &Credentials,
) -> Json {
    use nr_common::session_manager::{resolve_session_id, to_numeric_session_id};

    let project_id = credentials
        .psd_str("projectId")
        .map(String::from)
        .unwrap_or_else(AntigravityExecutor::generate_project_id);

    let mut body = body.clone();

    // Google rejects `stream_options` on non-streaming generateContent.
    if !stream {
        if let Some(obj) = body.as_object_mut() {
            obj.remove("stream_options");
        }
    }

    // ─── Image generation: a completely different request shape ───
    if is_image_model(model) {
        let image_config = AntigravityExecutor::parse_image_config(model);
        let clean_model = regex::Regex::new(r"-\d+x\d+$")
            .map(|re| re.replace(model, "").into_owned())
            .unwrap_or_else(|_| model.to_string());

        let src_contents = body
            .get("request")
            .and_then(|r| r.get("contents"))
            .or_else(|| body.get("contents"))
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default();

        let mut contents = Vec::new();
        for c in &src_contents {
            let mut image_parts = Vec::new();
            for p in c.get("parts").and_then(Json::as_array).into_iter().flatten() {
                if p.get("text").is_some() {
                    image_parts.push(json!({ "text": p["text"] }));
                } else if p.get("inlineData").and_then(|i| i.get("data")).is_some() {
                    image_parts.push(json!({ "inlineData": p["inlineData"] }));
                }
            }
            if !image_parts.is_empty() {
                let role = c.get("role").and_then(Json::as_str).unwrap_or("user");
                contents.push(json!({ "role": role, "parts": image_parts }));
            }
        }

        let connection = credentials
            .psd_str("email")
            .or_else(|| credentials.connection_id());
        let session_id = resolve_session_id(None, &body, connection, None, "antigravity");

        let request = json!({
            "contents": contents,
            "generationConfig": {
                "temperature": 1.0,
                "topP": 0.95,
                "topK": 40,
                "maxOutputTokens": 8192,
                "imageConfig": { "aspectRatio": image_config },
            },
            "sessionId": session_id,
        });

        let request_id = build_ide_request_id(&body, credentials, &clean_model, "image_gen");
        return json!({
            "project": project_id,
            "model": clean_model,
            "userAgent": "antigravity",
            "requestType": "image_gen",
            "requestId": request_id,
            "request": request,
        });
    }

    // ─── Standard (agent) request ───
    let raw_session_id = body
        .get("request")
        .and_then(|r| r.get("sessionId"))
        .and_then(Json::as_str)
        .map(String::from)
        .unwrap_or_else(|| {
            let connection = credentials
                .psd_str("email")
                .or_else(|| credentials.connection_id());
            resolve_session_id(None, &body, connection, None, "antigravity")
        });
    let session_id = to_numeric_session_id(&raw_session_id).unwrap_or(raw_session_id);

    let raw_contents = body
        .get("request")
        .and_then(|r| r.get("contents"))
        .and_then(Json::as_array)
        .cloned()
        .unwrap_or_default();

    let mut contents = Vec::new();
    for c in raw_contents {
        let mut role = c.get("role").and_then(Json::as_str).unwrap_or("user").to_string();
        let parts_in = c.get("parts").and_then(Json::as_array).cloned().unwrap_or_default();

        // functionResponse turns must be role "user" for Claude models.
        if parts_in.iter().any(|p| p.get("functionResponse").is_some()) {
            role = "user".to_string();
        }

        // Drop thought-only parts; Gemini 3+ needs thoughtSignature on functionCall parts.
        let kept: Vec<Json> = parts_in
            .into_iter()
            .filter(|p| {
                let has_call = p.get("functionCall").is_some();
                if p.get("thought").and_then(Json::as_bool).unwrap_or(false) && !has_call {
                    return false;
                }
                if p.get("thoughtSignature").is_some() && !has_call && p.get("text").is_none() {
                    return false;
                }
                true
            })
            .collect();

        let mut first_call_seen = false;
        let mut parts = Vec::with_capacity(kept.len());
        for mut p in kept {
            if p.get("functionCall").is_none() {
                parts.push(p);
                continue;
            }
            let call_id = p
                .get("functionCall")
                .and_then(|f| f.get("id"))
                .and_then(Json::as_str)
                .map(String::from);
            let cached = call_id.as_deref().and_then(|id| {
                nr_common::thought_signature_store::get_sync(
                    id,
                    Some(&session_id),
                    body.get("model").and_then(Json::as_str).or(Some(model)),
                )
            });
            let existing = p.get("thoughtSignature").and_then(Json::as_str).map(String::from);
            let fallback = if first_call_seen {
                None
            } else {
                Some(nr_data::export(
                    "open-sse/config/defaultThinkingSignature",
                    "DEFAULT_THINKING_AG_SIGNATURE",
                )
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default())
                .filter(|s| !s.is_empty())
            };
            first_call_seen = true;

            if let Some(sig) = existing.or(cached).or(fallback) {
                p["thoughtSignature"] = Json::String(sig);
            } else if p.get("thoughtSignature").is_some() {
                // Unsigned sibling call: strip the stale signature.
                if let Some(obj) = p.as_object_mut() {
                    obj.remove("thoughtSignature");
                }
            }
            parts.push(p);
        }

        let mut normalized = c.clone();
        normalized["role"] = Json::String(role);
        normalized["parts"] = Json::Array(parts);
        contents.push(normalized);
    }

    let contents = nr_translator::formats::gemini::normalize_gemini_contents(&contents, Some(model));

    // ─── Sanitize tools ───
    let mut tools: Option<Vec<Json>> = None;
    if let Some(groups) = body
        .get("request")
        .and_then(|r| r.get("tools"))
        .and_then(Json::as_array)
    {
        let mut seen = HashSet::new();
        let mut declarations = Vec::new();
        for group in groups {
            for f in group
                .get("functionDeclarations")
                .and_then(Json::as_array)
                .into_iter()
                .flatten()
            {
                let name = AntigravityExecutor::sanitize_function_name(
                    f.get("name").and_then(Json::as_str).unwrap_or(""),
                );
                if !seen.insert(name.clone()) {
                    continue;
                }
                let mut decl = f.clone();
                decl["name"] = Json::String(name);
                decl["parameters"] = match f.get("parameters") {
                    Some(p) => p.clone(),
                    None => json!({
                        "type": "object",
                        "properties": { "reason": { "type": "string", "description": "Brief explanation" } },
                        "required": ["reason"]
                    }),
                };
                declarations.push(decl);
            }
        }
        tools = Some(if declarations.is_empty() {
            Vec::new()
        } else {
            vec![json!({ "functionDeclarations": declarations })]
        });
    }

    // Request without tools/toolConfig, with blacklisted fields stripped.
    let mut request = body
        .get("request")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if let Some(obj) = request.as_object_mut() {
        obj.remove("tools");
        obj.remove("toolConfig");
        strip_blacklisted(obj);
    }
    rewrite_system_instruction(&mut request);

    // Clamp maxOutputTokens.
    let cfg = request.get("generationConfig").cloned().unwrap_or_else(|| json!({}));
    let mut generation_config = cfg;
    if let Some(max) = generation_config.get("maxOutputTokens").and_then(Json::as_i64) {
        if max > MAX_ANTIGRAVITY_OUTPUT_TOKENS {
            generation_config["maxOutputTokens"] = json!(MAX_ANTIGRAVITY_OUTPUT_TOKENS);
        }
    }

    if let Some(obj) = request.as_object_mut() {
        obj.insert("generationConfig".into(), generation_config);
        obj.insert("contents".into(), Json::Array(contents));
        obj.insert("sessionId".into(), Json::String(session_id.clone()));
        obj.remove("safetySettings");
        match &tools {
            Some(t) if !t.is_empty() => {
                obj.insert("tools".into(), Json::Array(t.clone()));
                obj.insert(
                    "toolConfig".into(),
                    json!({ "functionCallingConfig": { "mode": "VALIDATED" } }),
                );
            }
            Some(_) => {
                obj.remove("tools");
            }
            None => {}
        }
    }

    // Strip thinking fields that clients set at the body root.
    // The official client omits requestType on the agent path.
    if let Some(obj) = body.as_object_mut() {
        strip_blacklisted(obj);
        obj.remove("requestType");
    }

    let request_id = build_ide_request_id(&body, credentials, model, "agent");
    let _ = exec;
    let model_out = body
        .get("model")
        .and_then(Json::as_str)
        .unwrap_or(model)
        .to_string();

    if let Some(body_obj) = body.as_object_mut() {
        body_obj.insert("project".into(), Json::String(project_id));
        body_obj.insert("model".into(), Json::String(model_out));
        body_obj.insert("userAgent".into(), Json::String("antigravity".into()));
        body_obj.insert("requestId".into(), Json::String(request_id));
        body_obj.insert("request".into(), request);
    }

    body
}

/// Check if model is an image generation model
fn is_image_model(model: &str) -> bool {
    let m_lower = model.to_lowercase();
    m_lower.contains("image") || m_lower.contains("imagen") || m_lower.contains("image-generation")
}

pub struct AntigravityExecutor {
    provider: String,
    config: &'static nr_core::Json,
}

impl AntigravityExecutor {
    pub fn new(provider: &str) -> Self {
        let config = nr_data::provider(provider)
            .or_else(|| nr_data::provider("antigravity"))
            .unwrap_or_else(|| Box::leak(Box::new(Json::Object(serde_json::Map::new()))));
        Self {
            provider: provider.to_string(),
            config,
        }
    }

    /// Sanitize function name for Gemini: [a-zA-Z_][a-zA-Z0-9_.:\-]{0,63}
    fn sanitize_function_name(name: &str) -> String {
        if name.is_empty() {
            return "_unknown".to_string();
        }
        let mut s = name.replace(
            |c: char| !c.is_alphanumeric() && c != '_' && c != '.' && c != ':' && c != '-',
            "_",
        );
        if !s.starts_with(|c: char| c.is_alphabetic() || c == '_') {
            s.insert(0, '_');
        }
        s.chars().take(64).collect()
    }

    /// Parse aspect ratio from model name (e.g. "...-16x9" → "16:9")
    fn parse_image_config(model: &str) -> String {
        if let Some(caps) = regex::Regex::new(r"(\d+)x(\d+)$")
            .ok()
            .and_then(|re| re.captures(model))
        {
            if let (Ok(w), Ok(h)) = (caps[1].parse::<i32>(), caps[2].parse::<i32>()) {
                if w <= 16 && h <= 16 {
                    return format!("{}:{}", w, h);
                }
                // Compute GCD for aspect ratio reduction
                fn gcd(a: i32, b: i32) -> i32 {
                    if b == 0 {
                        a
                    } else {
                        gcd(b, a % b)
                    }
                }
                let d = gcd(w, h);
                return format!("{}:{}", w / d, h / d);
            }
        }
        "1:1".to_string()
    }

    /// Generate a project ID (used when credentials don't provide one)
    fn generate_project_id() -> String {
        let adjectives = ["useful", "bright", "swift", "calm", "bold"];
        let nouns = ["fuze", "wave", "spark", "flow", "core"];
        let adj = adjectives[rand::random::<usize>() % adjectives.len()];
        let noun = nouns[rand::random::<usize>() % nouns.len()];
        let suffix = uuid::Uuid::new_v4().to_string()[0..5].to_string();
        format!("{}-{}-{}", adj, noun, suffix)
    }

    /// Check if error should be retried
    fn is_transient_error(status: u16, message: &str) -> bool {
        if status == 429 || status == 503 {
            return true;
        }
        if status >= 500 && status < 600 {
            return true;
        }

        let message_lower = message.to_lowercase();
        let transient_patterns = [
            "high traffic",
            "agent execution terminated due to error",
            "capacity",
            "temporarily unavailable",
            "timeout",
            "stream ended",
            "stream closed",
            "stream terminated",
            "stream interrupted",
            "empty response",
        ];

        transient_patterns
            .iter()
            .any(|p| message_lower.contains(p))
    }

    /// Parse Retry-After header (seconds or HTTP-date)
    fn parse_retry_header(value: &str) -> Option<u64> {
        // Try parsing as seconds
        if let Ok(seconds) = value.parse::<u64>() {
            if seconds > 0 {
                return Some(seconds * 1000);
            }
        }

        // Try parsing as HTTP-date
        if let Ok(date_time) = httpdate::parse_http_date(value) {
            if let Ok(dur) = date_time.duration_since(SystemTime::now()) {
                return Some(dur.as_millis() as u64);
            }
        }

        None
    }

    /// Parse error response to extract quota reset time
    fn extract_quota_reset_time(error_json: &Json) -> Option<u64> {
        // Check for ErrorInfo/RetryInfo details
        if let Some(Json::Object(err)) = error_json.get("error") {
            if let Some(Json::Array(details)) = err.get("details") {
                for detail in details {
                    if let Json::Object(d) = detail {
                        // Check quotaResetTimeStamp
                        if let Some(Json::String(ts)) = d.get("metadata")
                            .and_then(|m| {
                                if let Json::Object(mo) = m {
                                    mo.get("quotaResetTimeStamp")
                                } else {
                                    None
                                }
                            })
                        {
                            if let Ok(ms) = ts.parse::<u64>() {
                                if ms > SystemTime::now()
                                    .duration_since(UNIX_EPOCH)
                                    .map(|d| d.as_millis() as u64)
                                    .unwrap_or(0)
                                {
                                    return Some(ms);
                                }
                            }
                        }

                        // Check retryDelay
                        if let Some(Json::String(delay_str)) = d.get("retryDelay") {
                            if let Ok(sec) = delay_str.parse::<f64>() {
                                if sec > 0.0 {
                                    let ms = (SystemTime::now()
                                        .duration_since(UNIX_EPOCH)
                                        .map(|d| d.as_millis() as u64)
                                        .unwrap_or(0))
                                        + (sec * 1000.0) as u64;
                                    if ms > SystemTime::now()
                                        .duration_since(UNIX_EPOCH)
                                        .map(|d| d.as_millis() as u64)
                                        .unwrap_or(0)
                                    {
                                        return Some(ms);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        None
    }

    /// Cloak tools by renaming client tools with _ide suffix
    /// Returns (cloaked_body, tool_name_map)
    fn cloak_tools(body: &Json, client_tool: Option<&str>) -> (Json, Option<IndexMap<String, String>>) {
        let is_copilot = client_tool == Some("github-copilot");
        let mut tool_name_map = IndexMap::new();

        let request_obj = match body.get("request") {
            Some(Json::Object(obj)) => obj.clone(),
            _ => {
                // No request object, return as-is
                return (body.clone(), None);
            }
        };

        // Get tools from request
        let tools = match request_obj.get("tools") {
            Some(Json::Array(t)) => t.clone(),
            _ => return (body.clone(), None), // No tools to cloak
        };

        // Collect all AG default tool names
        let ag_defaults: HashSet<&str> = AG_DECOY_TOOL_NAMES.iter().copied().collect();

        // Process client tools
        let mut all_declarations = Vec::new();
        let mut seen_names = HashSet::new();

        for tool_group in tools {
            if let Json::Object(group) = tool_group {
                if let Some(Json::Array(funcs)) = group.get("functionDeclarations") {
                    for func in funcs {
                        if let Json::Object(f) = func {
                            if let Some(Json::String(name)) = f.get("name") {
                                // For GitHub Copilot, skip duplicate native AG tool names
                                if is_copilot && ag_defaults.contains(name.as_str()) {
                                    continue;
                                }

                                // Preserve native AG names for non-Copilot clients
                                if ag_defaults.contains(name.as_str()) {
                                    seen_names.insert(name.clone());
                                    all_declarations.push(func.clone());
                                    continue;
                                }

                                // Rename client tools with suffix
                                let suffixed = format!("{}{}", name, AG_TOOL_SUFFIX);
                                tool_name_map.insert(suffixed.clone(), name.clone());
                                seen_names.insert(suffixed.clone());

                                let mut renamed = f.clone();
                                renamed.insert("name".into(), Json::String(suffixed));
                                all_declarations.push(Json::Object(renamed));
                            }
                        }
                    }
                }
            }
        }

        // Add AG decoy tools
        for decoy_name in AG_DECOY_TOOL_NAMES {
            if !seen_names.contains(*decoy_name) {
                seen_names.insert(decoy_name.to_string());
                all_declarations.push(json!({
                    "name": decoy_name,
                    "description": "This tool is currently unavailable.",
                    "parameters": { "type": "OBJECT", "properties": {}, "required": [] }
                }));
            }
        }

        // Rename tool names in contents
        let mut cloaked_contents = Vec::new();
        if let Some(Json::Array(contents)) = request_obj.get("contents") {
            for content in contents {
                if let Json::Object(mut c) = content.clone() {
                    if let Some(Json::Array(parts)) = c.get_mut("parts") {
                        for part in parts {
                            if let Json::Object(p) = part {
                                // Rename functionCall.name
                                if let Some(Json::Object(fc)) = p.get("functionCall") {
                                    if let Some(Json::String(fc_name)) = fc.get("name") {
                                        if !ag_defaults.contains(fc_name.as_str()) {
                                            let mut renamed_fc = fc.clone();
                                            renamed_fc.insert(
                                                "name".into(),
                                                Json::String(format!("{}{}", fc_name, AG_TOOL_SUFFIX)),
                                            );
                                            p.insert("functionCall".into(), Json::Object(renamed_fc));
                                        }
                                    }
                                }
                                // Rename functionResponse.name
                                if let Some(Json::Object(fr)) = p.get("functionResponse") {
                                    if let Some(Json::String(fr_name)) = fr.get("name") {
                                        if !ag_defaults.contains(fr_name.as_str()) {
                                            let mut renamed_fr = fr.clone();
                                            renamed_fr.insert(
                                                "name".into(),
                                                Json::String(format!("{}{}", fr_name, AG_TOOL_SUFFIX)),
                                            );
                                            p.insert("functionResponse".into(), Json::Object(renamed_fr));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    cloaked_contents.push(Json::Object(c));
                } else {
                    cloaked_contents.push(content.clone());
                }
            }
        }

        // Build cloaked request object
        let mut cloaked_request = request_obj.clone();
        if !all_declarations.is_empty() {
            cloaked_request.insert(
                "tools".into(),
                Json::Array(vec![json!({
                    "functionDeclarations": all_declarations
                })]),
            );
        }
        if !cloaked_contents.is_empty() {
            cloaked_request.insert("contents".into(), Json::Array(cloaked_contents));
        }

        // Build cloaked body
        let mut cloaked_body = body.clone();
        if let Some(obj) = cloaked_body.as_object_mut() {
            obj.insert("request".into(), Json::Object(cloaked_request));
        }

        let map_option = if tool_name_map.is_empty() {
            None
        } else {
            Some(tool_name_map)
        };

        (cloaked_body, map_option)
    }
}

impl BaseHooks for AntigravityExecutor {
    fn provider(&self) -> &str {
        "antigravity"
    }

    fn config(&self) -> &nr_core::Json {
        self.config
    }

    /// Image generation must use non-streaming `generateContent`.
    fn build_url(&self, model: &str, stream: bool, url_index: usize, _creds: &Credentials) -> Result<String> {
        let urls = BaseHooks::base_urls(self);
        let base = urls.get(url_index).or_else(|| urls.first()).cloned().unwrap_or_default();
        let base = base.trim_end_matches('/');
        let force_non_stream = is_image_model(model);
        let action = if stream && !force_non_stream {
            "streamGenerateContent?alt=sse"
        } else {
            "generateContent"
        };
        Ok(format!("{base}/v1internal:{action}"))
    }

    fn build_headers(&self, creds: &Credentials, _stream: bool, _url: &str, _model: &str, _body: &Json) -> Result<HeaderList> {
        let mut headers = HeaderList::new();
        headers.insert("Content-Type".into(), "application/json".into());
        if let Some(token) = creds.access_token() {
            headers.insert("Authorization".into(), format!("Bearer {token}"));
        }
        let ua = self.config.get("headers")
            .and_then(|h| h.get("User-Agent"))
            .and_then(Json::as_str)
            .unwrap_or("antigravity");
        headers.insert("User-Agent".into(), ua.to_string());
        Ok(headers)
    }

    fn transform_request(&self, model: &str, body: &Json, stream: bool, creds: &Credentials) -> Result<Json> {
        Ok(crate::antigravity::transform_request(self, model, body, stream, creds))
    }
}

#[async_trait]
impl Executor for AntigravityExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn no_auth(&self) -> bool {
        false
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        base_execute(self, req).await
    }
}

pub fn create(provider: &str) -> Arc<dyn Executor> {
    Arc::new(AntigravityExecutor::new(provider))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_function_name_valid() {
        assert_eq!(
            AntigravityExecutor::sanitize_function_name("hello_world"),
            "hello_world"
        );
        assert_eq!(
            AntigravityExecutor::sanitize_function_name("my.function"),
            "my.function"
        );
    }

    #[test]
    fn sanitize_function_name_starts_with_digit() {
        let result = AntigravityExecutor::sanitize_function_name("123func");
        assert!(result.starts_with('_'));
    }

    #[test]
    fn sanitize_function_name_special_chars() {
        let result = AntigravityExecutor::sanitize_function_name("func!@#$%");
        assert!(!result.contains('!'));
    }

    #[test]
    fn parse_image_config_ratio() {
        assert_eq!(AntigravityExecutor::parse_image_config("model-16x9"), "16:9");
        assert_eq!(AntigravityExecutor::parse_image_config("model-1024x768"), "4:3");
        assert_eq!(
            AntigravityExecutor::parse_image_config("model-1280x720"),
            "16:9"
        );
    }

    #[test]
    fn is_image_model_detection() {
        assert!(is_image_model("gemini-3.1-image"));
        assert!(is_image_model("imagen-3"));
        assert!(!is_image_model("gemini-3.1-flash"));
    }

    #[test]
    fn antigravity_executor_creates() {
        let executor = AntigravityExecutor::new("antigravity");
        assert_eq!(nr_core::Executor::provider(&executor), "antigravity");
    }

    #[test]
    fn uuid_from_seed_consistent() {
        let uuid1 = uuid_from_seed("test-seed");
        let uuid2 = uuid_from_seed("test-seed");
        assert_eq!(uuid1, uuid2);
    }

    #[test]
    fn parse_retry_from_error_message_hours() {
        let ms = parse_retry_from_error_message("Your quota will reset after 2h30m15s");
        assert!(ms.is_some());
        let ms_val = ms.unwrap();
        // 2h30m15s = 2*3600*1000 + 30*60*1000 + 15*1000 = 9015000
        assert!(ms_val >= 9000000 && ms_val <= 9030000);
    }

    #[test]
    fn is_transient_error_429() {
        assert!(AntigravityExecutor::is_transient_error(429, "rate limited"));
    }

    #[test]
    fn is_transient_error_503() {
        assert!(AntigravityExecutor::is_transient_error(503, "unavailable"));
    }

    #[test]
    fn is_transient_error_timeout() {
        assert!(AntigravityExecutor::is_transient_error(
            500,
            "timeout occurred"
        ));
    }

    #[test]
    fn is_transient_error_not_client_error() {
        assert!(!AntigravityExecutor::is_transient_error(
            400,
            "bad request"
        ));
    }
}
