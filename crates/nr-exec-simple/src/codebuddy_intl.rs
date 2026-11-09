//! Port of open-sse/executors/codebuddy-intl.js.
//! International CodeBuddy: like codebuddy-cn but with stable RFC-4122-v5 UUIDs from session keys.

use std::sync::Arc;
use async_trait::async_trait;
use nr_core::{Credentials, ExecRequest, ExecResult, Executor, Json, Result};
use nr_core::http::HeaderList;
use nr_exec_base::{base::{base_execute, BaseHooks}, DefaultExecutor};
use sha1::{Sha1, Digest};

pub struct CodeBuddyIntlExecutor {
    default: DefaultExecutor,
}

impl CodeBuddyIntlExecutor {
    pub fn new() -> Self {
        Self { default: DefaultExecutor::new("codebuddy-intl") }
    }

    /// Generate RFC-4122-v5-shaped UUID from SHA1 hash of session key
    fn stable_uuid_from_key(key: &str) -> String {
        let mut hasher = Sha1::new();
        hasher.update(key.as_bytes());
        let hash_hex = hex::encode(hasher.finalize());

        // Extract components and set version/variant bits per RFC-4122
        let time_high = (u32::from_str_radix(&hash_hex[12..16], 16).unwrap_or(0) & 0x0fff) | 0x5000;
        let clock_seq = (u32::from_str_radix(&hash_hex[16..20], 16).unwrap_or(0) & 0x3fff) | 0x8000;

        format!(
            "{}-{}-{:04x}-{:04x}-{}",
            &hash_hex[0..8],
            &hash_hex[8..12],
            time_high,
            clock_seq,
            &hash_hex[20..32]
        )
    }

    /// Read header case-insensitively
    fn read_header(headers: &HeaderList, name: &str) -> Option<String> {
        headers.get(name).map(|s| s.trim().to_string())
            .or_else(|| {
                // Case-insensitive fallback if HeaderList doesn't do it
                let lower = name.to_lowercase();
                for (k, v) in headers.iter() {
                    if k.to_lowercase() == lower {
                        return Some(v.trim().to_string());
                    }
                }
                None
            })
    }

    /// Flatten content to string for pattern matching
    fn flatten_content(content: &Json) -> String {
        match content {
            Json::String(s) => s.clone(),
            Json::Array(arr) => {
                arr.iter()
                    .filter_map(|b| b.get("text").and_then(Json::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            _ => String::new(),
        }
    }

    /// Detect and replace agent system prompts
    fn sanitize_system_prompts(&self, messages: &mut Vec<Json>) {
        let agent_pattern = regex::Regex::new(
            r"(?i)you are claude code|claude.?code.+official.+cli|anthropic.+official.+cli|anxthxropic.+official.+cli|you are (?:cursor|windsurf|cline|aider|continue|copilot|cody)|you are an? (?:ai )?(?:coding |code )?agent|cc_entrypoint\s*=\s*(?:cli|vscode|jetbrains|gui)|claude.?code.+issues|give feedback.+claude.?code|you are .{0,30}(?:powerful )?ai agent|orchestration capabilities|OhMyOpenCode|<agent-identity>|<Role>|<Behavior_Instructions>"
        ).unwrap_or_default();

        let neutral = "You are a helpful AI assistant that helps with software engineering tasks.";

        for msg in messages.iter_mut() {
            if let Json::Object(obj) = msg {
                if obj.get("role").and_then(Json::as_str) != Some("system") {
                    continue;
                }

                let text = Self::flatten_content(obj.get("content").unwrap_or(&Json::Null));
                if text.is_empty() || text.len() <= 2000 {
                    if !agent_pattern.is_match(&text) {
                        continue;
                    }
                }

                // Replace with neutral prompt
                match obj.get("content") {
                    Some(Json::String(_)) => {
                        obj.insert("content".into(), Json::String(neutral.into()));
                    }
                    Some(Json::Array(_)) => {
                        obj.insert("content".into(), Json::Array(vec![
                            serde_json::json!({"type": "text", "text": neutral})
                        ]));
                    }
                    _ => {}
                }
            }
        }
    }
}

#[async_trait]
impl BaseHooks for CodeBuddyIntlExecutor {
    fn provider(&self) -> &str {
        "codebuddy-intl"
    }

    fn config(&self) -> &Json {
        self.default.config()
    }

    fn build_url(&self, model: &str, stream: bool, url_index: usize, creds: &Credentials) -> Result<String> {
        self.default.build_url(model, stream, url_index, creds)
    }

    fn build_headers(&self, creds: &Credentials, stream: bool, url: &str, model: &str, body: &Json) -> Result<HeaderList> {
        let mut headers = self.default.build_headers(creds, stream, url, model, body)?;

        // Derive stable conversation ID from session key
        let session_key = body.get("session_id")
            .or_else(|| body.get("prompt_cache_key"))
            .or_else(|| body.get("conversation_id"))
            .and_then(Json::as_str)
            .map(|s| s.to_string());

        let first_user = if let Some(Json::Array(messages)) = body.get("messages") {
            messages.iter()
                .find(|m| m.get("role").and_then(Json::as_str) == Some("user"))
                .cloned()
        } else {
            None
        };

        let user_text = if let Some(Json::Object(msg)) = first_user {
            match msg.get("content") {
                Some(Json::String(s)) => s.clone(),
                Some(Json::Array(arr)) => {
                    arr.iter()
                        .filter_map(|b| b.get("text").and_then(Json::as_str))
                        .collect::<Vec<_>>()
                        .join("\n")
                }
                _ => String::new(),
            }
        } else {
            String::new()
        };

        let conversation_key = session_key
            .or_else(|| if !user_text.is_empty() { Some(format!("cb:{}", user_text)) } else { None });

        // Set conversation headers with stable UUIDs
        if !Self::read_header(&headers, "X-Conversation-ID").is_some() {
            let conv_id = conversation_key
                .as_ref()
                .map(|k| Self::stable_uuid_from_key(k))
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            headers.insert("X-Conversation-ID".into(), conv_id);
        }

        let req_id = Self::read_header(&headers, "X-Conversation-Request-ID")
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        if !Self::read_header(&headers, "X-Conversation-Request-ID").is_some() {
            headers.insert("X-Conversation-Request-ID".into(), req_id.clone());
        }
        if !Self::read_header(&headers, "X-Root-Request-ID").is_some() {
            headers.insert("X-Root-Request-ID".into(), req_id.clone());
        }

        let message_id = Self::read_header(&headers, "X-Conversation-Message-ID")
            .or_else(|| Self::read_header(&headers, "X-Request-ID"))
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        if !Self::read_header(&headers, "X-Conversation-Message-ID").is_some() {
            headers.insert("X-Conversation-Message-ID".into(), message_id.clone());
        }
        if !Self::read_header(&headers, "X-Request-ID").is_some() {
            headers.insert("X-Request-ID".into(), message_id);
        }

        Ok(headers)
    }

    fn transform_request(&self, model: &str, body: &Json, _stream: bool, creds: &Credentials) -> Result<Json> {
        let mut result = self.default.transform_request(model, body, true, creds)?;

        // Force stream = true
        result["stream"] = Json::Bool(true);

        // Filter agent system prompts
        if let Some(Json::Array(messages)) = result.get_mut("messages") {
            self.sanitize_system_prompts(messages);
        }

        // Reasoning mapping
        let eff = result.get("reasoning_effort").and_then(Json::as_str);
        if eff == Some("none") || eff == Some("off") {
            result.as_object_mut().map(|o| o.remove("reasoning_effort"));
        } else if eff.is_some() {
            result["reasoning_summary"] = Json::String("auto".into());
        }

        Ok(result)
    }
}

#[async_trait]
impl Executor for CodeBuddyIntlExecutor {
    fn provider(&self) -> &str {
        "codebuddy-intl"
    }

    fn no_auth(&self) -> bool {
        BaseHooks::no_auth(self)
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        base_execute(self, req).await
    }
}

pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(CodeBuddyIntlExecutor::new())
}
