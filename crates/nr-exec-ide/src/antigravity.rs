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
use nr_core::{ExecRequest, ExecResult, Executor, Json, JsonMap, Result};
use nr_exec_base::base::BaseHooks;
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
            .unwrap_or_else(|| Box::leak(Box::new(Json::Object(IndexMap::new()))));
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
        cloaked_body.insert("request".into(), Json::Object(cloaked_request));

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
        // TODO(dedupe): Implement full antigravity flow using DefaultExecutor's base
        // along with transformRequest, buildHeaders, and tool cloaking.
        // For now, delegate to default executor which handles the basic execution.
        nr_exec_base::default_executor::create("antigravity")
            .execute(req)
            .await
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
        assert_eq!(executor.provider(), "antigravity");
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
