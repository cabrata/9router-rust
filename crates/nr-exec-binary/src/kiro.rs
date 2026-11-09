//! Port of open-sse/executors/kiro.js
//! Kiro (AWS CodeWhisperer) executor with EventStream support and integrity gating.

use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;
use futures::stream::StreamExt;
use nr_core::http::HeaderList;
use nr_core::{Credentials, ExecRequest, ExecResult, Json};
use nr_exec_base::BaseHooks;
use regex::Regex;
use serde_json::{json, Value as JsonValue};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::aws_event_stream::{crc32, parse_event_frame, EventFrame, EventHeaderValue};

// Constants from kiro.js
const KIRO_REPAIR_BUFFER_MAX_BYTES: usize = 8 * 1024 * 1024;
const KIRO_REPAIR_HEARTBEAT_MS: u64 = 10_000;
const KIRO_SHORT_FINAL_MAX_CHARS: usize = 800;
const EVENTSTREAM_MAX_MESSAGE_BYTES: usize = 100 * 1024 * 1024;
const EVENTSTREAM_MAX_HEADERS_BYTES: usize = 8 * 1024;

lazy_static::lazy_static! {
    // Regex patterns from kiro.js
    static ref SHORT_FUTURE_ACTION: Regex = Regex::new(
        r#"(?x:^(?:(?:(?:現在|接著|接下來|下一步)[，,:：\s]*(?:我(?:只)?(?:會|要|將|再)?\s*)?|我只再)(?:補|查|確認|驗證|追(?:查|蹤)?|繼續|檢查|測試)|我(?:會|要|將)(?:再|重新)?(?:補(?:齊|查)?|抓取|查(?:詢)?|確認|驗證|追(?:查|蹤)?|繼續|檢查|測試)|(?:(?:next|now|then)\b[\s,:-]*)?(?:i(?:'ll| will| am going to| need to)|let me)\s+(?:verify|check|confirm|validate|investigate|trace|continue|follow up|test)\b)"#
    ).unwrap();
    static ref OBSERVED_TRAILING_FUTURE_ACTION: Regex = Regex::new(
        r#"(?x:^目前證據顯示[\s\S]{1,700}[。.!?；;]\s*最後補查\s+504\s+access\s+log[，,]\s*確認\s+host[／/]路徑與是否為集中流量[。.!]?$)"#
    ).unwrap();
    static ref ENGLISH_FUTURE_ACTION: Regex = Regex::new(
        r#"(?ix:^(?:(?:next|now|then)\b[\s,:-]*)?(?:i(?:'ll| will| am going to| need to)|let me)\s+(?:verify|check|confirm|validate|investigate|trace|continue|follow up|test)\b)"#
    ).unwrap();
    static ref ENGLISH_RESULT_CLAUSE: Regex = Regex::new(
        r#"(?ix:[:;\n]|[.!?]\s+\S|\b(?:status|checksum|response|deployment)\s+(?:is|are|was|were|matches?|equals?|returned)\b)"#
    ).unwrap();
    static ref CHINESE_FUTURE_ACTION: Regex = Regex::new(
        r"^(?:(?:現在|接著|接下來|下一步)[，,:：\s]*(?:我(?:只)?(?:會|要|將|再)?\s*)?|我只再|我(?:會|要|將)(?:再|重新)?)(?:補|抓取|查|確認|驗證|追|繼續|檢查|測試)"
    ).unwrap();
    static ref CHINESE_RESULT_CLAUSE: Regex = Regex::new(
        r"(?:[。！？]\s*\S|(?:版本|狀態|回應|結果|部署|校驗碼)(?:是|為|等於|顯示))"
    ).unwrap();
    static ref USER_WAIT: Regex = Regex::new(
        r#"(?ix:請(?:你|先)|你(?:先|需要|可以|提供|確認|批准|允許)|等待(?:你|使用者)|等你|核准|同意|授權|\b(?:after|when|once)\s+you\b|\byour\s+(?:approval|confirmation|permission|input)\b|\bwait(?:ing)?\s+for\s+you\b|\bplease\s+(?:approve|confirm|provide|send)\b)"#
    ).unwrap();
    static ref COMPLETED_FINAL: Regex = Regex::new(
        r#"(?ix:已(?:經)?完成|完成(?:了|驗證|確認)|修復完成|確認無誤|驗證(?:完成|通過)|測試(?:均)?通過|結論|總結|\b(?:done|completed|fixed|verified|confirmed|passed|in conclusion|summary)\b|\b(?:is|are) complete\b)"#
    ).unwrap();
    static ref RESULT_EVIDENCE: Regex = Regex::new(
        r#"(?ix:顯示|發現|因此|成功|失敗|正常|無錯誤|沒有錯誤|\b(?:found|shows?|showed|because|therefore|succeeded|failed|healthy|green|no errors?)\b)"#
    ).unwrap();

    // Service regions per kiro.js
    static ref KIRO_SERVICE_REGIONS: HashMap<&'static str, std::collections::HashSet<&'static str>> = {
        let mut m = HashMap::new();
        m.insert("q", vec!["us-east-1", "eu-central-1"].into_iter().collect());
        m.insert("runtime", vec!["us-east-1", "eu-central-1"].into_iter().collect());
        m.insert("codewhisperer", vec!["us-east-1"].into_iter().collect());
        m
    };

    // Kiro event types we process
    static ref KIRO_EVENT_TYPES: std::collections::HashSet<&'static str> = {
        vec![
            "assistantResponseEvent",
            "reasoningContentEvent",
            "codeEvent",
            "toolUseEvent",
            "messageStopEvent",
            "metadataEvent",
            "MetadataEvent",
            "contextUsageEvent",
            "meteringEvent",
            "metricsEvent"
        ].into_iter().collect()
    };

    // Stop reasons that allow truncation with output
    static ref KIRO_TRUNCATION_STOP_REASONS: std::collections::HashSet<&'static str> = {
        vec!["model_context_window_exceeded", "max_tokens"].into_iter().collect()
    };
}

// CodeWhisperer target from kiro.js config
const KIRO_CODEWHISPERER_TARGET: &str = "AmazonCodeWhisperer.GenerateAssistant";

pub struct KiroExecutor {
    provider: String,
    config: Json,
}

impl KiroExecutor {
    pub fn new(provider_id: &str) -> Self {
        let config = nr_data::provider(provider_id).cloned().unwrap_or_default();
        Self {
            provider: provider_id.to_string(),
            config,
        }
    }

    fn build_headers_impl(
        &self,
        creds: &Credentials,
        _stream: bool,
        url: &str,
    ) -> HeaderList {
        let mut headers = HeaderList::new();
        headers.insert("Content-Type".into(), "application/json".into());
        headers.insert("Amz-Sdk-Request".into(), "attempt=1; max=3".into());
        headers.insert("Amz-Sdk-Invocation-Id".into(), Uuid::new_v4().to_string());

        // Determine auth method from providerSpecificData
        if let Some(psd) = creds.psd() {
            let auth_method = psd.get("authMethod").and_then(|v| v.as_str());
            let is_api_key = auth_method == Some("api_key");
            let is_external_idp = auth_method == Some("external_idp");

            // API-key auth: send as Bearer token with TokenType header
            if is_api_key {
                if let Some(api_key) = psd.get("apiKey").and_then(|v| v.as_str()).or_else(|| {
                    creds.access_token()
                }) {
                    headers.insert("Authorization".into(), format!("Bearer {}", api_key));
                    headers.insert("TokenType".into(), "API_KEY".into());
                }
            } else if let Some(token) = creds.access_token() {
                headers.insert("Authorization".into(), format!("Bearer {}", token));
                if is_external_idp {
                    headers.insert("TokenType".into(), "EXTERNAL_IDP".into());
                }
            }

            // CLIRO parity headers
            if let Some(token) = creds.access_token() {
                headers.insert("x-amz-sso-bearer".into(), token.to_string());
            }
        } else if let Some(token) = creds.access_token() {
            headers.insert("Authorization".into(), format!("Bearer {}", token));
        }

        headers.insert("x-amzn-kiro-agent-mode".into(), "spec".into());
        headers.insert("x-amzn-codewhisperer-machine-id".into(), "kiro-desktop".into());

        // Add profile ARN header if present
        if let Some(psd) = creds.psd() {
            if let Some(profile_arn) = psd.get("profileArn").and_then(|v| v.as_str()) {
                headers.insert("x-amzn-codewhisperer-profile-arn".into(), profile_arn.to_string());
            }
        }

        // X-Amz-Target for CodeWhisperer endpoint
        if url.contains("://codewhisperer.") {
            headers.insert("X-Amz-Target".into(), KIRO_CODEWHISPERER_TARGET.to_string());
        }

        headers
    }

    fn get_ordered_base_urls(&self, creds: &Credentials) -> Vec<String> {
        let base_urls = self.base_urls();
        let auth_method = creds
            .psd()
            .and_then(|psd| psd.get("authMethod").and_then(|v| v.as_str()));

        // Extract and validate region
        let requested_region = creds
            .psd()
            .and_then(|psd| psd.get("region").and_then(|v| v.as_str()))
            .unwrap_or("us-east-1")
            .trim();

        // Regionalize AWS URLs if the region is valid for the service
        let regionalize = |url: &str| -> String {
            if !url.contains("amazonaws.com") {
                return url.to_string();
            }
            if let Some(caps) = regex::Regex::new(r"//([a-z]+)\.[a-z0-9-]+\.amazonaws\.com")
                .ok()
                .and_then(|re| re.captures(url))
            {
                if let Some(service) = caps.get(1).map(|m| m.as_str()) {
                    if let Some(regions) = KIRO_SERVICE_REGIONS.get(service) {
                        if regions.contains(requested_region) {
                            return url.replace(
                                &format!("{}.[a-z0-9-]+.amazonaws.com", service),
                                &format!("{}.{}.amazonaws.com", service, requested_region),
                            );
                        }
                    }
                }
            }
            url.to_string()
        };

        // Split into q (api-key priority) and others
        let mut amazon: Vec<String> = base_urls
            .iter()
            .filter(|u| u.contains("amazonaws.com"))
            .map(|u| regionalize(u))
            .collect();
        let others: Vec<String> = base_urls
            .iter()
            .filter(|u| !u.contains("amazonaws.com"))
            .cloned()
            .collect();

        let q: Vec<String> = amazon.iter().filter(|u| u.contains("://q.")).cloned().collect();
        let remaining: Vec<String> = amazon
            .iter()
            .filter(|u| !u.contains("://q."))
            .cloned()
            .collect();

        if !q.is_empty() {
            [q, remaining, others].concat()
        } else {
            [amazon, others].concat()
        }
    }

    fn normalize_stop_reason(&self, value: Option<&str>) -> Option<String> {
        let reason = value
            .map(|s| {
                s.trim()
                    .chars()
                    .fold(String::new(), |mut acc, c| {
                        if acc.chars().last().map_or(false, |ch| ch.is_lowercase())
                            && c.is_uppercase()
                        {
                            acc.push('_');
                        }
                        acc.push(c.to_lowercase().next().unwrap_or(c));
                        acc
                    })
                    .replace(|c: char| c.is_whitespace() || c == '-', "_")
            })
            .unwrap_or_default();

        match reason.as_str() {
            "endturn" | "end_turn" | "stop" | "stop_sequence" => Some("end_turn".to_string()),
            "tooluse" | "tool_use" | "tool_calls" => Some("tool_use".to_string()),
            "maxtokens" | "max_tokens" | "max_output_tokens" | "length" => Some("max_tokens".to_string()),
            "" => None,
            _ => Some(reason),
        }
    }

    fn stop_disposition(&self, stop_reason: Option<&str>, has_tool_calls: bool) -> &'static str {
        if let Some(reason) = stop_reason {
            if ["malformed_model_output", "invalid_model_output"].contains(&reason) {
                return "retryable_protocol_failure";
            }
            if ["cancelled", "pause_turn", "model_context_window_exceeded"].contains(&reason) {
                return "terminal_incomplete";
            }
            if reason == "refusal"
                || reason.contains("content") && reason.contains("filter")
                || reason.contains("guardrail")
                || reason.contains("safety")
                || reason.contains("policy")
                || reason.contains("blocked")
            {
                return "terminal_refusal";
            }
            if reason == "max_tokens" {
                return if has_tool_calls { "terminal_incomplete" } else { "length" };
            }
            if !["end_turn", "tool_use"].contains(&reason) {
                return "unknown_failure";
            }
        }
        if has_tool_calls || stop_reason == Some("tool_use") {
            return "tool_use";
        }
        if stop_reason.is_none() || stop_reason == Some("end_turn") {
            return "complete";
        }
        "unknown_failure"
    }

    fn is_ellipsis_only(&self, value: Option<&str>) -> bool {
        match value.map(|s| s.trim()) {
            Some("...") | Some("…") => true,
            _ => false,
        }
    }

    fn is_short_future_action(&self, value: Option<&str>) -> bool {
        let text = value
            .map(|s| s.trim().replace("'", "'"))
            .unwrap_or_default();

        if text.is_empty() {
            return false;
        }

        if text.len() > KIRO_SHORT_FINAL_MAX_CHARS {
            return false;
        }

        if OBSERVED_TRAILING_FUTURE_ACTION.is_match(&text) {
            return true;
        }

        if ENGLISH_FUTURE_ACTION.is_match(&text) && ENGLISH_RESULT_CLAUSE.is_match(&text) {
            return false;
        }

        if CHINESE_FUTURE_ACTION.is_match(&text) && CHINESE_RESULT_CLAUSE.is_match(&text) {
            return false;
        }

        SHORT_FUTURE_ACTION.is_match(&text)
            && !USER_WAIT.is_match(&text)
            && !COMPLETED_FINAL.is_match(&text)
            && !RESULT_EVIDENCE.is_match(&text)
    }
}

#[async_trait]
impl BaseHooks for KiroExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn config(&self) -> &Json {
        &self.config
    }

    fn build_url(
        &self,
        _model: &str,
        _stream: bool,
        url_index: usize,
        creds: &Credentials,
    ) -> nr_core::Result<String> {
        let base_urls = self.get_ordered_base_urls(creds);
        Ok(base_urls
            .get(url_index)
            .cloned()
            .or_else(|| base_urls.first().cloned())
            .unwrap_or_else(|| "https://runtime.us-east-1.kiro.dev".to_string()))
    }

    fn build_headers(
        &self,
        creds: &Credentials,
        stream: bool,
        url: &str,
        _model: &str,
        _body: &Json,
    ) -> nr_core::Result<HeaderList> {
        Ok(self.build_headers_impl(creds, stream, url))
    }

    fn should_retry(&self, status: u16, url_index: usize) -> bool {
        let has_fallback = url_index + 1 < self.base_urls().len();
        // Base retry logic: 429, 5xx
        let base_should_retry = matches!(status, 429 | 500..=599);
        // Kiro-specific fallback statuses
        let kiro_fallback_statuses = [401, 403, 502, 503, 504];
        base_should_retry || (has_fallback && kiro_fallback_statuses.contains(&status))
    }
}

#[async_trait]
impl nr_core::Executor for KiroExecutor {
    fn provider(&self) -> &str {
        BaseHooks::provider(self)
    }

    fn no_auth(&self) -> bool {
        BaseHooks::no_auth(self)
    }

    async fn execute(&self, req: ExecRequest<'_>) -> nr_core::Result<ExecResult> {
        let requested_model = req.model.to_string();
        let result = nr_exec_base::base_execute(self, req).await?;

        // Successful Kiro answers are binary AWS EventStream frames; convert
        // them to OpenAI SSE. Errors stay untransformed so the handler can read
        // the body and trigger account fallback.
        if result.response.status == 200 {
            let transformed = transform_event_stream_to_sse(result.response, &requested_model, None);
            return Ok(ExecResult { response: transformed, ..result });
        }

        Ok(result)
    }
}

pub fn create(provider: &str) -> Arc<dyn nr_core::Executor> {
    Arc::new(KiroExecutor::new(provider))
}

// ==================== EventStream → SSE (port of transformEventStreamToSSE) ====================

/// `strip_synthetic_suffixes`+normalize: the capability model behind a Kiro id.
fn kiro_capability_model(model: &str) -> String {
    nr_data::kiro_constants::resolve_kiro_model(model)
        .get("upstream")
        .and_then(Json::as_str)
        .unwrap_or(model)
        .to_string()
}

fn kiro_context_window(model: &str) -> i64 {
    nr_data::capabilities::get_capabilities_for_model(Some("kiro"), Some(&kiro_capability_model(model)))
        .get("contextWindow")
        .and_then(Json::as_i64)
        .unwrap_or(200_000)
}

fn sse_chunk_with_usage(
    id: &str,
    created: i64,
    model: &str,
    delta: &Json,
    finish_reason: Option<&str>,
    usage: Option<&Json>,
) -> String {
    let mut payload = serde_json::json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish_reason }],
    });
    if let Some(usage) = usage {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("usage".into(), usage.clone());
        }
    }
    nr_common::sse::sse_chunk(&payload)
}

/// `emitDelta`: the first delta of a turn carries `role: "assistant"`.
fn emit_delta(
    state: &mut KiroStreamState,
    sink: &mut Vec<String>,
    id: &str,
    created: i64,
    model: &str,
    delta: Json,
) {
    let delta = if state.chunk_index == 0 {
        match delta {
            Json::Object(mut map) => {
                let mut with_role = serde_json::Map::new();
                with_role.insert("role".into(), Json::String("assistant".into()));
                for (k, v) in std::mem::take(&mut map) {
                    with_role.insert(k, v);
                }
                Json::Object(with_role)
            }
            other => other,
        }
    } else {
        delta
    };
    state.chunk_index += 1;
    sink.push(sse_chunk_with_usage(id, created, model, &delta, None, None));
}

/// `encodeSSEError`: an error frame plus the terminal `[DONE]`.
fn encode_sse_error(code: &str, message: &str, details: &Json) -> String {
    let payload = serde_json::json!({ "error": {
        "message": message,
        "type": "upstream_error",
        "code": code,
        "details": details,
    }});
    format!("data: {payload}\n\n{}", nr_common::sse_constants::SSE_DONE)
}

/// `normalizeStopReason` (module-level in kiro.js, not a method on this type).
fn normalize_stop_reason_value(value: Option<&str>) -> Option<String> {
    let raw = value.unwrap_or("");
    let mut normalized = String::new();
    let mut prev_lower = false;
    for ch in raw.trim().chars() {
        if prev_lower && ch.is_uppercase() {
            normalized.push('_');
        }
        for lower in ch.to_lowercase() {
            normalized.push(lower);
        }
        prev_lower = ch.is_lowercase();
    }
    // JS: `.replace(/[\s-]+/g, "_")` collapses runs, then `.toLowerCase()`.
    let mut collapsed = String::new();
    let mut in_run = false;
    for ch in normalized.chars() {
        if ch.is_whitespace() || ch == '-' {
            if !in_run {
                collapsed.push('_');
                in_run = true;
            }
        } else {
            collapsed.push(ch);
            in_run = false;
        }
    }
    let normalized = collapsed;

    match normalized.as_str() {
        "endturn" | "end_turn" | "stop" | "stop_sequence" => Some("end_turn".to_string()),
        "tooluse" | "tool_use" | "tool_calls" => Some("tool_use".to_string()),
        "maxtokens" | "max_tokens" | "max_output_tokens" | "length" => Some("max_tokens".to_string()),
        "" => None,
        other => Some(other.to_string()),
    }
}

fn stop_disposition_value(stop_reason: Option<&str>, has_tool_calls: bool) -> &'static str {
    if let Some(reason) = stop_reason {
        if reason == "malformed_model_output" || reason == "invalid_model_output" {
            return "retryable_protocol_failure";
        }
        if reason == "cancelled" || reason == "pause_turn" || reason == "model_context_window_exceeded" {
            return "terminal_incomplete";
        }
        if reason == "refusal"
            || (reason.contains("content") && reason.contains("filter"))
            || reason.contains("guardrail")
            || reason.contains("safety")
            || reason.contains("policy")
            || reason.contains("blocked")
        {
            return "terminal_refusal";
        }
        if reason == "max_tokens" {
            return if has_tool_calls { "terminal_incomplete" } else { "length" };
        }
        if reason != "end_turn" && reason != "tool_use" {
            return "unknown_failure";
        }
    }
    if has_tool_calls || stop_reason == Some("tool_use") {
        return "tool_use";
    }
    if stop_reason.is_none() || stop_reason == Some("end_turn") {
        return "complete";
    }
    "unknown_failure"
}

fn stop_severity(reason: &str) -> u8 {
    match stop_disposition_value(Some(reason), false) {
        "terminal_refusal" => 6,
        "terminal_incomplete" => 5,
        "unknown_failure" => 4,
        "retryable_protocol_failure" => 3,
        "length" => 2,
        _ => 1,
    }
}

/// `mergeStopReason`: the more severe of the two wins.
fn merge_stop_reason(current: Option<String>, incoming: Option<String>) -> Option<String> {
    let incoming = match incoming {
        Some(incoming) => incoming,
        None => return current,
    };
    match current {
        None => Some(incoming),
        Some(current) => {
            if stop_severity(&incoming) > stop_severity(&current) {
                Some(incoming)
            } else {
                Some(current)
            }
        }
    }
}

/// Buffered tool call being assembled from `toolUseEvent` fragments.
struct PendingTool {
    id: String,
    name: String,
    input_kind: Option<&'static str>,
    input_chunks: Vec<String>,
    input_object: Option<Json>,
    input_bytes: usize,
}

struct KiroStreamState {
    tools: Vec<PendingTool>,
    buffered_tool_bytes: usize,
    has_text: bool,
    has_reasoning: bool,
    has_code: bool,
    has_tool_calls: bool,
    saw_tool_use: bool,
    explicit_stop: bool,
    stop_reason: Option<String>,
    total_content_length: i64,
    context_usage_percentage: f64,
    has_context_usage: bool,
    has_metering: bool,
    usage: Option<Json>,
    in_thinking: bool,
    tool_validation_error: Option<String>,
    chunk_index: u64,
}

impl KiroStreamState {
    fn new() -> Self {
        Self {
            tools: Vec::new(),
            buffered_tool_bytes: 0,
            has_text: false,
            has_reasoning: false,
            has_code: false,
            has_tool_calls: false,
            saw_tool_use: false,
            explicit_stop: false,
            stop_reason: None,
            total_content_length: 0,
            context_usage_percentage: 0.0,
            has_context_usage: false,
            has_metering: false,
            usage: None,
            in_thinking: false,
            tool_validation_error: None,
            chunk_index: 0,
        }
    }
}

fn event_header_string(frame: &EventFrame, name: &str) -> Option<String> {
    match frame.headers.get(name) {
        Some(EventHeaderValue::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// `appendToolInput`: strings accumulate in order, objects accumulate per field.
fn append_tool_input(tool: &mut PendingTool, input: Option<&Json>) -> std::result::Result<(), String> {
    let input = match input {
        Some(input) if !input.is_null() => input,
        _ => return Ok(()),
    };
    match input {
        Json::String(text) => {
            if tool.input_kind.is_some() && tool.input_kind != Some("string") {
                return Err("Kiro tool input changed fragment type".into());
            }
            tool.input_kind = Some("string");
            tool.input_chunks.push(text.clone());
        }
        Json::Object(map) => {
            if tool.input_kind.is_some() && tool.input_kind != Some("object") {
                return Err("Kiro tool input changed fragment type".into());
            }
            tool.input_kind = Some("object");
            let object = match tool.input_object.take() {
                Some(Json::Object(existing)) => {
                    let mut merged = existing;
                    for (k, v) in map {
                        merged.insert(k.clone(), v.clone());
                    }
                    merged
                }
                _ => map.clone(),
            };
            tool.input_bytes = serde_json::to_string(&Json::Object(object.clone()))
                .map(|s| s.len())
                .unwrap_or(0);
            tool.input_object = Some(Json::Object(object));
        }
        _ => return Err("Kiro tool input must be a JSON object".into()),
    }
    Ok(())
}

/// `parsedToolInput`: the fully assembled argument object.
fn parsed_tool_input(tool: &PendingTool) -> std::result::Result<Json, String> {
    match tool.input_kind {
        None => Err("Kiro tool call is missing input".into()),
        Some("object") => Ok(tool.input_object.clone().unwrap_or(Json::Object(Default::default()))),
        Some(_) => {
            let joined = tool.input_chunks.join("");
            let parsed: Json = serde_json::from_str(&joined)
                .map_err(|e| format!("Kiro tool input must be valid object JSON ({e})"))?;
            match parsed {
                Json::Object(_) => Ok(parsed),
                _ => Err("Kiro tool input must be valid object JSON (not an object)".into()),
            }
        }
    }
}

/// `emitTools`: flush buffered calls, dropping unusable ones individually.
fn emit_tools(state: &mut KiroStreamState, sink: &mut Vec<String>, id: &str, created: i64, model: &str) -> std::result::Result<(), String> {
    let mut tools = std::mem::take(&mut state.tools);
    let mut index = 0u64;
    for tool in tools.drain(..) {
        let input = match parsed_tool_input(&tool) {
            Ok(input) => {
                if tool.name == "tool_call" {
                    let ok = input.get("name").and_then(Json::as_str).map(|n| !n.trim().is_empty()).unwrap_or(false)
                        && input.get("arguments").is_some();
                    if !ok {
                        state.tool_validation_error.get_or_insert_with(|| {
                            "Invalid Kiro tool_call payload: missing nested MCP tool name or arguments".to_string()
                        });
                        continue;
                    }
                }
                input
            }
            Err(err) => {
                state.tool_validation_error.get_or_insert(err);
                continue;
            }
        };

        let call_index = index;
        index += 1;
        let first = serde_json::json!({
            "index": call_index,
            "id": tool.id,
            "type": "function",
            "function": { "name": tool.name, "arguments": "" },
        });
        emit_delta(state, sink, id, created, model, serde_json::json!({ "tool_calls": [first] }));
        let serialized = input.to_string();
        let second = serde_json::json!({
            "index": call_index,
            "function": { "arguments": serialized },
        });
        emit_delta(state, sink, id, created, model, serde_json::json!({ "tool_calls": [second] }));
        // Tool arguments bill as output tokens like any other completion bytes.
        state.total_content_length += tool.name.len() as i64 + serialized.len() as i64;
        state.has_tool_calls = true;
    }
    state.buffered_tool_bytes = 0;

    if state.stop_reason.as_deref() == Some("tool_use")
        && !state.has_tool_calls
        && !state.has_text
        && !state.has_reasoning
        && !state.has_code
    {
        return Err("Kiro tool_use stop reason did not include a complete tool call".into());
    }
    Ok(())
}

/// `processEvent`: fold one EventStream frame into the state and SS[[sink]].
fn process_event(
    frame: &EventFrame,
    state: &mut KiroStreamState,
    sink: &mut Vec<String>,
    id: &str,
    created: i64,
    model: &str,
) -> std::result::Result<bool, (String, String)> {
    let message_type = event_header_string(frame, ":message-type").unwrap_or_default();
    if message_type == "error" || message_type == "exception" {
        let message = frame
            .payload
            .as_ref()
            .and_then(|p| p.get("message"))
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_string();
        return Err((
            "kiro_upstream_eventstream_error".to_string(),
            if message.is_empty() {
                format!("Kiro upstream sent an EventStream {message_type}")
            } else {
                message
            },
        ));
    }

    let event_type = event_header_string(frame, ":event-type").unwrap_or_default();
    let payload = frame.payload.clone().unwrap_or(Json::Null);

    match event_type.as_str() {
        "assistantResponseEvent" => {
            let raw = payload.get("content").and_then(Json::as_str).unwrap_or("");
            let mut content = raw.to_string();
            if state.in_thinking {
                match content.find("</thinking>") {
                    Some(end) => {
                        state.in_thinking = false;
                        content = content[end + "</thinking>".len()..].to_string();
                        if let Some(stripped) = content.strip_prefix('\n') {
                            content = stripped.to_string();
                        }
                    }
                    None => content = String::new(),
                }
            } else if let Some(start) = content.find("<thinking>") {
                if let Some(rel_end) = content[start + "<thinking>".len()..].find("</thinking>") {
                    let end = start + "<thinking>".len() + rel_end;
                    let tail = content[end + "</thinking>".len()..].to_string();
                    let tail = tail.strip_prefix('\n').unwrap_or(&tail).to_string();
                    content = format!("{}{}", &content[..start], tail);
                } else {
                    state.in_thinking = true;
                    content = content[..start].to_string();
                }
            }
            if !content.is_empty() || !state.has_reasoning {
                if !content.is_empty() {
                    state.has_text = true;
                }
                state.total_content_length += content.len() as i64;
                emit_delta(state, sink, id, created, model, serde_json::json!({ "content": content }));
            }
        }
        "reasoningContentEvent" => {
            let value = payload
                .get("reasoningContentEvent")
                .cloned()
                .unwrap_or_else(|| payload.clone());
            let content = match &value {
                Json::String(s) => s.clone(),
                other => other
                    .get("text")
                    .and_then(Json::as_str)
                    .or_else(|| other.get("content").and_then(Json::as_str))
                    .unwrap_or("")
                    .to_string(),
            };
            if !content.is_empty() {
                state.has_reasoning = true;
                state.total_content_length += content.len() as i64;
                emit_delta(
                    state,
                    sink,
                    id,
                    created,
                    model,
                    serde_json::json!({ "reasoning_content": content }),
                );
            }
        }
        "codeEvent" => {
            if let Some(content) = payload.get("content").and_then(Json::as_str) {
                state.has_code = true;
                state.total_content_length += content.len() as i64;
                emit_delta(state, sink, id, created, model, serde_json::json!({ "content": content }));
            }
        }
        "toolUseEvent" => {
            state.saw_tool_use = true;
            let values = match &payload {
                Json::Array(items) => items.clone(),
                other => vec![other.clone()],
            };
            if values.first().map(|v| v.is_null()).unwrap_or(true) {
                return Err(("kiro_missing_terminal".into(), "Kiro toolUseEvent is empty".into()));
            }
            for value in values {
                let name = value.get("name").and_then(Json::as_str).unwrap_or("").trim().to_string();
                if name.is_empty() {
                    return Err((
                        "kiro_missing_terminal".into(),
                        "Kiro toolUseEvent is missing a tool name".into(),
                    ));
                }
                let id_value = value.get("toolUseId");
                let tool_id = match id_value {
                    None | Some(Json::Null) => format!("call_{created}_{}", state.tools.len() + 1),
                    Some(Json::String(s)) if !s.trim().is_empty() => s.clone(),
                    _ => {
                        return Err((
                            "kiro_missing_terminal".into(),
                            "Kiro toolUseEvent has an invalid toolUseId".into(),
                        ))
                    }
                };
                let position = state.tools.iter().position(|t| t.id == tool_id);
                let idx = match position {
                    Some(idx) => {
                        if state.tools[idx].name != name {
                            return Err((
                                "kiro_missing_terminal".into(),
                                "Kiro tool name changed between fragments".into(),
                            ));
                        }
                        idx
                    }
                    None => {
                        state.tools.push(PendingTool {
                            id: tool_id.clone(),
                            name: name.clone(),
                            input_kind: None,
                            input_chunks: Vec::new(),
                            input_object: None,
                            input_bytes: 0,
                        });
                        state.buffered_tool_bytes += tool_id.len() + name.len() + 32;
                        state.tools.len() - 1
                    }
                };
                append_tool_input(&mut state.tools[idx], value.get("input"))
                    .map_err(|e| ("kiro_missing_terminal".to_string(), e))?;
                // One rejected fragment must not drop the other complete calls.
                if state.buffered_tool_bytes > KIRO_REPAIR_BUFFER_MAX_BYTES / 2 {
                    return Err((
                        "kiro_integrity_buffer_exceeded".into(),
                        "Kiro buffered tool input exceeded the integrity memory bound".into(),
                    ));
                }
            }
        }
        "messageStopEvent" => {
            state.explicit_stop = true;
            let reason = normalize_stop_reason_value(
                payload
                    .get("stopReason")
                    .and_then(Json::as_str)
                    .or_else(|| payload.get("stop_reason").and_then(Json::as_str)),
            )
            .or_else(|| Some(if state.saw_tool_use { "tool_use".into() } else { "end_turn".into() }));
            state.stop_reason = merge_stop_reason(state.stop_reason.take(), reason);
        }
        "metadataEvent" | "MetadataEvent" => {
            let metadata = payload
                .get("metadataEvent")
                .or_else(|| payload.get("metadata"))
                .cloned()
                .unwrap_or_else(|| payload.clone());
            let reason = normalize_stop_reason_value(
                metadata
                    .get("stopReason")
                    .and_then(Json::as_str)
                    .or_else(|| metadata.get("stop_reason").and_then(Json::as_str)),
            );
            if reason.is_some() {
                state.explicit_stop = true;
                state.stop_reason = merge_stop_reason(state.stop_reason.take(), reason);
            }
        }
        "contextUsageEvent" => {
            if let Some(pct) = payload.get("contextUsagePercentage").and_then(Json::as_f64) {
                state.context_usage_percentage = pct;
                state.has_context_usage = true;
            }
        }
        "meteringEvent" => {
            state.has_metering = true;
            let metering = payload
                .get("meteringEvent")
                .cloned()
                .unwrap_or_else(|| payload.clone());
            let mut usage = state.usage.take().unwrap_or(Json::Object(Default::default()));
            if let Some(credits) = metering.get("usage").and_then(Json::as_f64) {
                usage["kiro_credits"] = Json::from(credits);
                usage["kiro_credit_unit"] = Json::String(
                    metering
                        .get("unit")
                        .and_then(Json::as_str)
                        .unwrap_or("credit")
                        .to_string(),
                );
            }
            state.usage = Some(usage);
        }
        "metricsEvent" => {
            let metrics = payload
                .get("metricsEvent")
                .cloned()
                .unwrap_or_else(|| payload.clone());
            let prompt = metrics.get("inputTokens").and_then(Json::as_f64).unwrap_or(0.0);
            let completion = metrics.get("outputTokens").and_then(Json::as_f64).unwrap_or(0.0);
            if prompt != 0.0 || completion != 0.0 {
                let mut usage = state.usage.take().unwrap_or(Json::Object(Default::default()));
                usage["prompt_tokens"] = Json::from(prompt);
                usage["completion_tokens"] = Json::from(completion);
                usage["total_tokens"] = Json::from(prompt + completion);
                let cache_read = metrics
                    .get("cacheReadInputTokens")
                    .or_else(|| metrics.get("cache_read_input_tokens"))
                    .and_then(Json::as_f64)
                    .unwrap_or(0.0);
                let cache_create = metrics
                    .get("cacheCreationInputTokens")
                    .or_else(|| metrics.get("cache_creation_input_tokens"))
                    .and_then(Json::as_f64)
                    .unwrap_or(0.0);
                if cache_read != 0.0 {
                    usage["cache_read_input_tokens"] = Json::from(cache_read);
                }
                if cache_create != 0.0 {
                    usage["cache_creation_input_tokens"] = Json::from(cache_create);
                }
                state.usage = Some(usage);
            }
        }
        _ => {}
    }

    Ok(true)
}

/// Port of `transformEventStreamToSSE`: decode binary AWS EventStream frames
/// and emit OpenAI-shaped SSE, with the same integrity gating.
pub fn transform_event_stream_to_sse(
    mut response: nr_core::http::UpstreamResponse,
    model: &str,
    max_raw_bytes: Option<usize>,
) -> nr_core::http::UpstreamResponse {
    use futures::{SinkExt, StreamExt};

    let id = format!("chatcmpl-{}", chrono::Utc::now().timestamp_millis());
    let created = chrono::Utc::now().timestamp();
    let context_window = kiro_context_window(model);
    let model = model.to_string();
    let max_raw_bytes = max_raw_bytes.unwrap_or(EVENTSTREAM_MAX_MESSAGE_BYTES);

    let (mut tx, rx) = futures::channel::mpsc::channel::<std::io::Result<Bytes>>(64);

    tokio::spawn(async move {
        let mut state = KiroStreamState::new();
        let mut buffer: Vec<u8> = Vec::new();
        let mut sink: Vec<String> = Vec::new();
        let mut terminal_error: Option<(String, String)> = None;

        'outer: while let Some(chunk) = response.body.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    terminal_error = Some((
                        "kiro_missing_terminal".into(),
                        format!("Kiro EventStream read failed: {e}"),
                    ));
                    break;
                }
            };
            if chunk.is_empty() {
                continue;
            }
            if buffer.len() + chunk.len() > max_raw_bytes {
                terminal_error = Some((
                    "kiro_missing_terminal".into(),
                    "Kiro EventStream buffered bytes exceed the protocol bound".into(),
                ));
                break;
            }
            buffer.extend_from_slice(&chunk);

            while buffer.len() >= 12 {
                let prelude_crc = u32::from_be_bytes([buffer[8], buffer[9], buffer[10], buffer[11]]);
                if prelude_crc != crc32(&buffer[..8]) {
                    terminal_error = Some((
                        "kiro_missing_terminal".into(),
                        "Kiro EventStream prelude CRC mismatch".into(),
                    ));
                    break 'outer;
                }
                let total_length = u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;
                let headers_length = u32::from_be_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]) as usize;
                if total_length < 16
                    || total_length > EVENTSTREAM_MAX_MESSAGE_BYTES
                    || headers_length > EVENTSTREAM_MAX_HEADERS_BYTES
                    || headers_length > total_length.saturating_sub(16)
                {
                    terminal_error = Some((
                        "kiro_missing_terminal".into(),
                        "Kiro EventStream frame bounds are invalid".into(),
                    ));
                    break 'outer;
                }
                if buffer.len() < total_length {
                    break;
                }
                let frame_bytes: Vec<u8> = buffer.drain(..total_length).collect();
                let frame = match parse_event_frame(&frame_bytes) {
                    Ok(frame) => frame,
                    Err(err) => {
                        terminal_error = Some(("kiro_missing_terminal".into(), err));
                        break 'outer;
                    }
                };
                if let Err((code, message)) =
                    process_event(&frame, &mut state, &mut sink, &id, created, &model)
                {
                    terminal_error = Some((code, message));
                    break 'outer;
                }
            }

            for frame in sink.drain(..) {
                if tx.send(Ok(Bytes::from(frame))).await.is_err() {
                    return;
                }
            }
        }

        // Flush anything the loop produced before it failed.
        for frame in sink.drain(..) {
            if tx.send(Ok(Bytes::from(frame))).await.is_err() {
                return;
            }
        }

        if let Some((code, message)) = terminal_error {
            let details = serde_json::json!({
                "terminal_provenance": "corrupt_eventstream_frame",
                "transport_state": "corrupt_frame",
                "stop_reason": state.stop_reason,
                "stop_disposition": stop_disposition_value(state.stop_reason.as_deref(), state.has_tool_calls),
                "response_state": if state.has_tool_calls {
                    "valid_tool"
                } else if state.has_text || state.has_reasoning || state.has_code {
                    "text_reasoning"
                } else if state.explicit_stop {
                    "explicit_stop"
                } else {
                    "no_semantic_output"
                },
                "incomplete_frame_bytes": buffer.len(),
            });
            let _ = tx.send(Ok(Bytes::from(encode_sse_error(&code, &message, &details)))).await;
            return;
        }

        // Truncated trailing frame: never pretend the turn completed.
        if !buffer.is_empty() {
            let details = serde_json::json!({
                "terminal_provenance": "incomplete_eventstream_frame",
                "transport_state": "incomplete_frame",
                "incomplete_frame_bytes": buffer.len(),
            });
            let _ = tx
                .send(Ok(Bytes::from(encode_sse_error(
                    "kiro_missing_terminal",
                    "Kiro EventStream ended with a truncated frame",
                    &details,
                ))))
                .await;
            return;
        }

        let declared = stop_disposition_value(state.stop_reason.as_deref(), state.saw_tool_use);
        let truncation_reason = state
            .stop_reason
            .as_deref()
            .map(|r| KIRO_TRUNCATION_STOP_REASONS.contains(r))
            .unwrap_or(false);
        // A truncated-but-usable answer keeps its output and closes as "length".
        let declared_truncated_after_output =
            declared == "terminal_incomplete" && truncation_reason && state.chunk_index > 0;

        if !declared_truncated_after_output
            && ["retryable_protocol_failure", "terminal_incomplete", "terminal_refusal", "unknown_failure"]
                .contains(&declared)
        {
            let code = match declared {
                "retryable_protocol_failure" => "kiro_retryable_protocol_failure",
                "terminal_refusal" => "kiro_terminal_refusal",
                "terminal_incomplete" => "kiro_terminal_incomplete",
                _ => "kiro_unknown_stop_reason",
            };
            let details = serde_json::json!({
                "terminal_provenance": "metadata_stop_reason",
                "transport_state": "clean_eof",
                "stop_reason": state.stop_reason,
                "stop_disposition": declared,
            });
            let message = format!(
                "Kiro ended with non-success stop reason: {}",
                state.stop_reason.clone().unwrap_or_default()
            );
            let _ = tx.send(Ok(Bytes::from(encode_sse_error(code, &message, &details)))).await;
            return;
        }

        if let Err(err) = emit_tools(&mut state, &mut sink, &id, created, &model) {
            let details = serde_json::json!({
                "terminal_provenance": "invalid_tool_call",
                "transport_state": "clean_eof",
                "stop_disposition": "retryable_protocol_failure",
            });
            let _ = tx
                .send(Ok(Bytes::from(encode_sse_error("invalid_kiro_tool_call", &err, &details))))
                .await;
            return;
        }
        for frame in sink.drain(..) {
            let _ = tx.send(Ok(Bytes::from(frame))).await;
        }

        // A declared tool turn with no usable call is only fatal when the turn
        // produced nothing else; text the client already saw is kept.
        if let Some(err) = state.tool_validation_error.clone() {
            if !state.has_tool_calls && !state.has_text && !state.has_reasoning && !state.has_code {
                let details = serde_json::json!({
                    "terminal_provenance": "invalid_tool_call",
                    "transport_state": "clean_eof",
                    "stop_disposition": "retryable_protocol_failure",
                });
                let _ = tx
                    .send(Ok(Bytes::from(encode_sse_error("invalid_kiro_tool_call", &err, &details))))
                    .await;
                return;
            }
        }

        let has_output = state.has_text || state.has_reasoning || state.has_code || state.has_tool_calls;
        if !has_output && !state.explicit_stop {
            let details = serde_json::json!({
                "terminal_provenance": "empty_response_eof",
                "transport_state": "clean_eof",
            });
            let _ = tx
                .send(Ok(Bytes::from(encode_sse_error(
                    "kiro_missing_terminal",
                    "Kiro EventStream ended without model output",
                    &details,
                ))))
                .await;
            return;
        }

        let disposition = stop_disposition_value(state.stop_reason.as_deref(), state.has_tool_calls);
        let truncated_after_output =
            disposition == "terminal_incomplete" && truncation_reason && state.chunk_index > 0;
        if !truncated_after_output
            && ["retryable_protocol_failure", "terminal_incomplete", "terminal_refusal", "unknown_failure"]
                .contains(&disposition)
        {
            let code = match disposition {
                "retryable_protocol_failure" => "kiro_retryable_protocol_failure",
                "terminal_refusal" => "kiro_terminal_refusal",
                "terminal_incomplete" => "kiro_terminal_incomplete",
                _ => "kiro_unknown_stop_reason",
            };
            let details = serde_json::json!({
                "terminal_provenance": "metadata_stop_reason",
                "transport_state": "clean_eof",
                "stop_reason": state.stop_reason,
                "stop_disposition": disposition,
            });
            let message = format!(
                "Kiro ended with non-success stop reason: {}",
                state.stop_reason.clone().unwrap_or_default()
            );
            let _ = tx.send(Ok(Bytes::from(encode_sse_error(code, &message, &details)))).await;
            return;
        }

        // Metering-only turns have no token counts; derive them from the context
        // percentage and the emitted content length.
        if state.has_metering
            && state.has_context_usage
            && state
                .usage
                .as_ref()
                .and_then(|u| u.get("total_tokens"))
                .is_none()
        {
            let completion = if state.total_content_length > 0 {
                (state.total_content_length / 4).max(1)
            } else {
                0
            };
            let prompt = (state.context_usage_percentage * context_window as f64 / 100.0).floor() as i64;
            let mut usage = state.usage.take().unwrap_or(Json::Object(Default::default()));
            usage["prompt_tokens"] = Json::from(prompt);
            usage["completion_tokens"] = Json::from(completion);
            usage["total_tokens"] = Json::from(prompt + completion);
            state.usage = Some(usage);
        }

        let finish_reason = if truncated_after_output {
            "length"
        } else if state.has_tool_calls {
            "tool_calls"
        } else if disposition == "length" {
            "length"
        } else {
            "stop"
        };
        let final_frame = sse_chunk_with_usage(
            &id,
            created,
            &model,
            &serde_json::json!({}),
            Some(finish_reason),
            state.usage.as_ref(),
        );
        let _ = tx.send(Ok(Bytes::from(final_frame))).await;
        let _ = tx
            .send(Ok(Bytes::from(nr_common::sse_constants::SSE_DONE)))
            .await;
    });

    let mut headers = http::HeaderMap::new();
    for (k, v) in nr_common::sse_constants::SSE_HEADERS {
        if let (Ok(name), Ok(value)) = (
            http::header::HeaderName::try_from(*k),
            http::HeaderValue::try_from(*v),
        ) {
            headers.insert(name, value);
        }
    }
    nr_core::http::UpstreamResponse::from_stream(200, headers, Box::pin(rx))
}

#[cfg(test)]
mod event_stream_tests {
    use super::*;
    use futures::StreamExt;

    fn event_type_header(event_type: &str) -> Vec<u8> {
        let mut headers = Vec::new();
        headers.push(0x00);
        headers.extend_from_slice(&(":event-type".len() as u16).to_be_bytes());
        headers.extend_from_slice(":event-type".as_bytes());
        headers.push(0x07);
        headers.extend_from_slice(&(event_type.len() as u16).to_be_bytes());
        headers.extend_from_slice(event_type.as_bytes());
        headers
    }

    fn build_frame(message_type: &str, event_type: &str, payload: &Json) -> Vec<u8> {
        let mut headers = Vec::new();
        for (name, value) in [(":message-type", message_type), (":event-type", event_type)] {
            headers.push(name.len() as u8);
            headers.extend_from_slice(name.as_bytes());
            headers.push(0x07); // string header value
            headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
            headers.extend_from_slice(value.as_bytes());
        }
        let payload_bytes = serde_json::to_vec(payload).unwrap_or_default();
        // Prelude (12) + headers + payload + message CRC (4).
        let total = 12 + headers.len() + payload_bytes.len() + 4;
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&(total as u32).to_be_bytes());
        out.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        // The prelude CRC covers only the 8 prelude bytes.
        out.extend_from_slice(&crc32(&out[..8]).to_be_bytes());
        out.extend_from_slice(&headers);
        out.extend_from_slice(&payload_bytes);
        // The message CRC covers the whole message up to (not including) itself.
        out.extend_from_slice(&crc32(&out).to_be_bytes());
        out
    }

    fn text_event(text: &str) -> Vec<u8> {
        build_frame("event", "assistantResponseEvent", &serde_json::json!({ "content": text }))
    }

    fn stop_event(reason: &str) -> Vec<u8> {
        build_frame("event", "messageStopEvent", &serde_json::json!({ "stopReason": reason }))
    }

    async fn collect(response: nr_core::http::UpstreamResponse) -> String {
        let mut out = String::new();
        let mut body = response.body;
        while let Some(chunk) = body.next().await {
            out.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        }
        out
    }

    #[tokio::test]
    async fn text_turn_emits_deltas_and_finishes_stop() {
        let body = [text_event("Hello"), stop_event("END_TURN")].concat();
        let response = nr_core::http::UpstreamResponse::from_bytes(200, http::HeaderMap::new(), body);
        let sse = collect(transform_event_stream_to_sse(response, "claude-sonnet-4.5", None)).await;
        assert!(sse.contains(r#""content":"Hello""#));
        assert!(sse.contains(r#""role":"assistant""#));
        assert!(sse.contains(r#""finish_reason":"stop""#));
        assert!(sse.ends_with(nr_common::sse_constants::SSE_DONE));
        assert!(!sse.contains("error"));
    }

    #[tokio::test]
    async fn multi_chunk_text_carries_role_once() {
        let body = [text_event("He"), text_event("y"), stop_event("END_TURN")].concat();
        let response = nr_core::http::UpstreamResponse::from_bytes(200, http::HeaderMap::new(), body);
        let sse = collect(transform_event_stream_to_sse(response, "claude-sonnet-4.5", None)).await;
        assert_eq!(sse.matches(r#""role":"assistant""#).count(), 1);
    }

    #[tokio::test]
    async fn reasoning_and_code_events_map_to_deltas() {
        let reasoning = build_frame(
            "event",
            "reasoningContentEvent",
            &serde_json::json!({ "reasoningContentEvent": { "text": "thinking" } }),
        );
        let code = build_frame("event", "codeEvent", &serde_json::json!({ "content": "fn main(){}" }));
        let body = [reasoning, code, stop_event("END_TURN")].concat();
        let response = nr_core::http::UpstreamResponse::from_bytes(200, http::HeaderMap::new(), body);
        let sse = collect(transform_event_stream_to_sse(response, "claude-sonnet-4.5", None)).await;
        assert!(sse.contains(r#""reasoning_content":"thinking""#));
        assert!(sse.contains(r#""content":"fn main(){}""#));
    }

    #[tokio::test]
    async fn tool_use_turn_emits_tool_calls_and_finish_reason() {
        let tool = build_frame(
            "event",
            "toolUseEvent",
            &serde_json::json!({
                "name": "tool_call",
                "toolUseId": "t1",
                "input": { "name": "Write", "arguments": { "path": "a.rs" } },
            }),
        );
        let body = [tool, stop_event("TOOL_USE")].concat();
        let response = nr_core::http::UpstreamResponse::from_bytes(200, http::HeaderMap::new(), body);
        let sse = collect(transform_event_stream_to_sse(response, "claude-sonnet-4.5", None)).await;
        assert!(sse.contains(r#""finish_reason":"tool_calls""#));
        assert!(sse.contains(r#""name":"tool_call""#));
        // Tool arguments ride as a JSON-encoded string inside the SSE delta.
        assert!(sse.contains(r#""arguments":"{\"name\":\"Write\""#), "got {sse}");
        assert!(sse.contains(r#"a.rs"#));
    }

    #[tokio::test]
    async fn refusal_stop_reason_surfaces_upstream_error() {
        let body = [text_event("no"), stop_event("REFUSAL")].concat();
        let response = nr_core::http::UpstreamResponse::from_bytes(200, http::HeaderMap::new(), body);
        let sse = collect(transform_event_stream_to_sse(response, "claude-sonnet-4.5", None)).await;
        assert!(sse.contains("kiro_terminal_refusal"));
        assert!(sse.contains(nr_common::sse_constants::SSE_DONE));
    }

    #[tokio::test]
    async fn max_tokens_after_output_closes_as_length() {
        let body = [text_event("partial"), stop_event("MAX_TOKENS")].concat();
        let response = nr_core::http::UpstreamResponse::from_bytes(200, http::HeaderMap::new(), body);
        let sse = collect(transform_event_stream_to_sse(response, "claude-sonnet-4.5", None)).await;
        assert!(sse.contains(r#""finish_reason":"length""#));
        assert!(!sse.contains("error"));
    }

    #[tokio::test]
    async fn error_frame_surfaces_upstream_error() {
        let body = build_frame("error", "InternalServerException", &serde_json::json!({
            "message": "boom",
        }));
        let response = nr_core::http::UpstreamResponse::from_bytes(200, http::HeaderMap::new(), body);
        let sse = collect(transform_event_stream_to_sse(response, "claude-sonnet-4.5", None)).await;
        assert!(sse.contains("kiro_upstream_eventstream_error"));
        assert!(sse.contains("boom"));
    }

    #[tokio::test]
    async fn corrupt_crc_surfaces_corrupt_eventstream_frame() {
        let mut body = text_event("hi");
        // Flip the message CRC (last 4 bytes).
        let last = body.len() - 1;
        body[last] ^= 0xff;
        let response = nr_core::http::UpstreamResponse::from_bytes(200, http::HeaderMap::new(), body);
        let sse = collect(transform_event_stream_to_sse(response, "claude-sonnet-4.5", None)).await;
        assert!(sse.contains("kiro_missing_terminal"));
        assert!(sse.contains("CRC") || sse.contains("truncated frame"));
    }

    #[tokio::test]
    async fn empty_stream_reports_missing_output() {
        let response = nr_core::http::UpstreamResponse::from_bytes(200, http::HeaderMap::new(), Vec::new());
        let sse = collect(transform_event_stream_to_sse(response, "claude-sonnet-4.5", None)).await;
        assert!(sse.contains("kiro_missing_terminal"));
        assert!(sse.contains("ended without model output"));
    }

    #[tokio::test]
    async fn metering_derives_usage_from_content() {
        let metering = build_frame("event", "meteringEvent", &serde_json::json!({ "usage": 2 }));
        let ctx = build_frame("event", "contextUsageEvent", &serde_json::json!({ "contextUsagePercentage": 10 }));
        let text = text_event(&"x".repeat(400));
        let body = [metering, ctx, text, stop_event("END_TURN")].concat();
        let response = nr_core::http::UpstreamResponse::from_bytes(200, http::HeaderMap::new(), body);
        let sse = collect(transform_event_stream_to_sse(response, "claude-sonnet-4.5", None)).await;
        assert!(sse.contains(r#""prompt_tokens":20000"#));
        assert!(sse.contains(r#""completion_tokens":100"#));
    }

    #[tokio::test]
    async fn thinking_tags_are_stripped_from_content() {
        let wrapped = text_event("<thinking>secret</thinking>answer");
        let body = [wrapped, stop_event("END_TURN")].concat();
        let response = nr_core::http::UpstreamResponse::from_bytes(200, http::HeaderMap::new(), body);
        let sse = collect(transform_event_stream_to_sse(response, "claude-sonnet-4.5", None)).await;
        assert!(sse.contains(r#""content":"answer""#));
        assert!(!sse.contains("secret"));
    }

    #[test]
    fn normalize_stop_reason_aliases() {
        assert_eq!(normalize_stop_reason_value(Some("END_TURN")), Some("end_turn".into()));
        assert_eq!(normalize_stop_reason_value(Some("tool_use")), Some("tool_use".into()));
        assert_eq!(normalize_stop_reason_value(Some("maxOutputTokens")), Some("max_tokens".into()));
        assert_eq!(normalize_stop_reason_value(Some("length")), Some("max_tokens".into()));
        assert_eq!(normalize_stop_reason_value(None), None);
        assert_eq!(normalize_stop_reason_value(Some("weird_reason")), Some("weird_reason".into()));
    }

    #[test]
    fn stop_disposition_mapping() {
        assert_eq!(stop_disposition_value(Some("end_turn"), false), "complete");
        assert_eq!(stop_disposition_value(None, false), "complete");
        assert_eq!(stop_disposition_value(Some("tool_use"), false), "tool_use");
        assert_eq!(stop_disposition_value(None, true), "tool_use");
        assert_eq!(stop_disposition_value(Some("refusal"), false), "terminal_refusal");
        assert_eq!(stop_disposition_value(Some("cancelled"), false), "terminal_incomplete");
        assert_eq!(stop_disposition_value(Some("max_tokens"), false), "length");
        assert_eq!(stop_disposition_value(Some("max_tokens"), true), "terminal_incomplete");
        assert_eq!(stop_disposition_value(Some("paused_for_input"), false), "unknown_failure");
    }

    #[test]
    fn merge_stop_reason_prefers_severity() {
        assert_eq!(merge_stop_reason(Some("end_turn".into()), Some("refusal".into())).as_deref(), Some("refusal"));
        assert_eq!(merge_stop_reason(Some("refusal".into()), Some("end_turn".into())).as_deref(), Some("refusal"));
        assert_eq!(merge_stop_reason(None, Some("end_turn".into())).as_deref(), Some("end_turn"));
        assert_eq!(merge_stop_reason(Some("end_turn".into()), None).as_deref(), Some("end_turn"));
    }

    #[test]
    fn context_window_defaults_to_200k() {
        assert_eq!(kiro_context_window("claude-sonnet-4.5"), 200_000);
    }
}

