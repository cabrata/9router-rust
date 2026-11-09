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

use crate::aws_event_stream::{crc32, parse_event_frame, EventFrame};

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
        if let Some(psd) = creds.provider_specific_data() {
            let auth_method = psd.get("authMethod").and_then(|v| v.as_str());
            let is_api_key = auth_method == Some("api_key");
            let is_external_idp = auth_method == Some("external_idp");

            // API-key auth: send as Bearer token with TokenType header
            if is_api_key {
                if let Some(api_key) = psd.get("apiKey").and_then(|v| v.as_str()).or_else(|| {
                    creds.access_token().map(|t| t.as_str())
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
                headers.insert("x-amz-sso-bearer".into(), token.clone());
            }
        } else if let Some(token) = creds.access_token() {
            headers.insert("Authorization".into(), format!("Bearer {}", token));
        }

        headers.insert("x-amzn-kiro-agent-mode".into(), "spec".into());
        headers.insert("x-amzn-codewhisperer-machine-id".into(), "kiro-desktop".into());

        // Add profile ARN header if present
        if let Some(psd) = creds.provider_specific_data() {
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
            .provider_specific_data()
            .and_then(|psd| psd.get("authMethod").and_then(|v| v.as_str()));

        // Extract and validate region
        let requested_region = creds
            .provider_specific_data()
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
        let result = nr_exec_base::base_execute(self, req).await?;

        // Handle EventStream transformation
        if result.response.status() == 200 {
            // TODO(exec-binary): Transform EventStream response to SSE
            // This requires:
            // - Reading response body as binary EventStream frames
            // - Parsing each frame via crate::aws_event_stream::parse_event_frame
            // - Converting to OpenAI SSE format
            // - Integrity gating (tool call repair) if env var not set
            // For now, pass through untransformed
        }

        Ok(result)
    }
}

pub fn create(provider: &str) -> Arc<dyn nr_core::Executor> {
    Arc::new(KiroExecutor::new(provider))
}
