//! Port of open-sse/executors/codebuddy-cn.js.
//! Tencent CodeBuddy executor: forces stream=true, filters agent prompts, adds conversation headers.

use std::sync::Arc;
use async_trait::async_trait;
use nr_core::{Credentials, ExecRequest, ExecResult, Executor, Json, Result};
use nr_core::http::HeaderList;
use nr_exec_base::{base::{base_execute, BaseHooks}, DefaultExecutor};

pub struct CodeBuddyCnExecutor {
    default: DefaultExecutor,
}

impl CodeBuddyCnExecutor {
    pub fn new() -> Self {
        Self { default: DefaultExecutor::new("codebuddy-cn") }
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

    /// Detect and replace agent system prompts with neutral message
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
impl BaseHooks for CodeBuddyCnExecutor {
    fn provider(&self) -> &str {
        "codebuddy-cn"
    }

    fn config(&self) -> &Json {
        self.default.config()
    }

    fn build_url(&self, model: &str, stream: bool, url_index: usize, creds: &Credentials) -> Result<String> {
        self.default.build_url(model, stream, url_index, creds)
    }

    fn build_headers(&self, creds: &Credentials, stream: bool, url: &str, model: &str, body: &Json) -> Result<HeaderList> {
        let mut headers = self.default.build_headers(creds, stream, url, model, body)?;

        // Add conversation headers for channel verification (X-Conversation-* family)
        let req_id = uuid::Uuid::new_v4().to_string();
        headers.insert("X-Request-ID".into(), req_id.clone());
        headers.insert("X-Conversation-ID".into(), uuid::Uuid::new_v4().to_string());
        headers.insert("X-Conversation-Request-ID".into(), req_id.clone());
        headers.insert("X-Conversation-Message-ID".into(), req_id);

        Ok(headers)
    }

    fn transform_request(&self, model: &str, body: &Json, _stream: bool, creds: &Credentials) -> Result<Json> {
        let mut result = self.default.transform_request(model, body, true, creds)?;

        // Force stream = true (Tencent rejects non-stream requests)
        result["stream"] = Json::Bool(true);

        // Filter agent system prompts
        if let Some(Json::Array(messages)) = result.get_mut("messages") {
            self.sanitize_system_prompts(messages);
        }

        // Reasoning mapping: CodeBuddy surfaces reasoning only with reasoning_summary:"auto"
        let eff = result.get("reasoning_effort").and_then(Json::as_str);
        if eff == Some("none") || eff == Some("off") {
            result.as_object_mut().map(|o| o.remove("reasoning_effort"));
        } else if eff.is_some() && eff != Some("none") {
            // Client asked for reasoning — add reasoning_summary so CodeBuddy surfaces it
            result["reasoning_summary"] = Json::String("auto".into());
        }

        Ok(result)
    }
}

#[async_trait]
impl Executor for CodeBuddyCnExecutor {
    fn provider(&self) -> &str {
        "codebuddy-cn"
    }

    fn no_auth(&self) -> bool {
        BaseHooks::no_auth(self)
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        base_execute(self, req).await
    }
}

pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(CodeBuddyCnExecutor::new())
}
