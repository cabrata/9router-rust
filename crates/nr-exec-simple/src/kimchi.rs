//! Port of open-sse/executors/kimchi.js.
//!
//! Kimchi executor: wraps DefaultExecutor to strip incompatible fields
//! and merge top-level system messages.

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use nr_core::{ExecRequest, ExecResult, Executor, Result, Json};
use nr_exec_base::base::{base_execute, BaseHooks};

static EMPTY: LazyLock<Json> = LazyLock::new(|| Json::Object(Default::default()));

const TOP_LEVEL_OPENAI_GATEWAY_DROPS: &[&str] = &[
    "anthropic_version",
    "anthropic_beta",
    "client_metadata",
    "mcp_servers",
    "stop_sequences",
    "thinking",
    "top_k",
];

const REASONING_PLACEHOLDER_MAX_LEN: usize = 8;

/// Convert system message to text, handling string or array formats.
fn system_to_text(system: &Json) -> String {
    match system {
        Json::String(s) => s.clone(),
        Json::Array(arr) => {
            arr.iter()
                .filter_map(|part| {
                    if let Json::String(s) = part {
                        Some(s.clone())
                    } else if let Some(obj) = part.as_object() {
                        obj.get("text").and_then(Json::as_str).map(String::from)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => String::new(),
    }
}

/// Merge top-level system message into messages array.
fn merge_top_level_system(body: &mut Json) {
    let system_text = body.get("system").map(system_to_text).filter(|s| !s.trim().is_empty());
    if system_text.is_none() {
        return;
    }

    if let Some(msgs) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        let text = system_text.unwrap();
        let existing = msgs.iter_mut().find(|msg| msg.get("role").and_then(Json::as_str) == Some("system"));

        if let Some(existing_msg) = existing {
            if let Some(content) = existing_msg.get_mut("content") {
                match content {
                    Json::String(s) => {
                        *s = format!("{}\n\n{}", text, s);
                    }
                    Json::Array(arr) => {
                        arr.insert(0, serde_json::json!({"type": "text", "text": text}));
                    }
                    _ => {}
                }
            }
        } else {
            msgs.insert(0, serde_json::json!({"role": "system", "content": text}));
        }
    }
}

/// Strip cache_control and other artifacts from messages.
fn strip_message_artifacts(body: &mut Json) {
    if let Some(msgs) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in msgs {
            if let Some(obj) = msg.as_object_mut() {
                obj.remove("cache_control");
            }
            if let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) {
                *content = content.iter().map(|part| {
                    if let Some(obj) = part.as_object() {
                        let mut cleaned = obj.clone();
                        cleaned.remove("cache_control");
                        cleaned.remove("signature");
                        Json::Object(cleaned)
                    } else {
                        part.clone()
                    }
                }).collect();
            }
        }
    }
}

/// Strip cache_control from tools.
fn strip_tool_artifacts(body: &mut Json) {
    if let Some(tools) = body.get_mut("tools").and_then(|t| t.as_array_mut()) {
        *tools = tools.iter().map(|tool| {
            if let Some(obj) = tool.as_object() {
                let mut cleaned = obj.clone();
                cleaned.remove("cache_control");
                Json::Object(cleaned)
            } else {
                tool.clone()
            }
        }).collect();
    }
}

/// Strip reasoning_content from assistant messages if it's longer than placeholder threshold.
pub fn strip_reasoning_content(body: &mut Json) {
    if let Some(msgs) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in msgs {
            if msg.get("role").and_then(Json::as_str) == Some("assistant") {
                if let Some(reasoning) = msg.get("reasoning_content").and_then(Json::as_str) {
                    if reasoning.len() > REASONING_PLACEHOLDER_MAX_LEN {
                        if let Some(obj) = msg.as_object_mut() {
                            obj.remove("reasoning_content");
                        }
                    }
                }
            }
        }
    }
}

/// Check if a model is Anthropic-backed (by name or metadata).
fn is_anthropic_backed_kimchi_model(model: &str) -> bool {
    let lower = model.to_lowercase();
    lower.contains("claude") || lower.contains("anthropic")
}

/// BaseHooks implementation for Kimchi.
struct KimchiHooks {
    config: &'static Json,
}

impl KimchiHooks {
    fn new() -> Self {
        let config = nr_data::provider("kimchi").or_else(|| nr_data::provider("openai")).unwrap_or(&EMPTY);
        Self { config }
    }

    /// Apply Kimchi-specific transformations to request body.
    fn apply_kimchi_transforms(&self, model: &str, body: &Json) -> Json {
        let mut transformed = body.clone();

        // Merge top-level system into messages
        merge_top_level_system(&mut transformed);

        // Drop OpenAI incompatible fields
        if let Some(obj) = transformed.as_object_mut() {
            for key in TOP_LEVEL_OPENAI_GATEWAY_DROPS {
                obj.remove(*key);
            }
            obj.remove("system");
        }

        // Drop reasoning fields for Anthropic-backed models
        if is_anthropic_backed_kimchi_model(model) {
            if let Some(obj) = transformed.as_object_mut() {
                obj.remove("reasoning_effort");
                obj.remove("reasoning");
                obj.remove("thinking");
            }
        }

        // Strip artifacts
        strip_message_artifacts(&mut transformed);
        strip_tool_artifacts(&mut transformed);
        strip_reasoning_content(&mut transformed);

        transformed
    }
}

impl BaseHooks for KimchiHooks {
    fn provider(&self) -> &str {
        "kimchi"
    }

    fn config(&self) -> &Json {
        self.config
    }

    fn transform_request(&self, model: &str, body: &Json, _stream: bool, _creds: &nr_core::Credentials) -> Result<Json> {
        Ok(self.apply_kimchi_transforms(model, body))
    }
}

struct KimchiExecutor;

#[async_trait]
impl Executor for KimchiExecutor {
    fn provider(&self) -> &str {
        "kimchi"
    }

    fn no_auth(&self) -> bool {
        let hooks = KimchiHooks::new();
        BaseHooks::no_auth(&hooks)
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        let hooks = KimchiHooks::new();
        base_execute(&hooks, req).await
    }
}

/// Executor for the canonical provider id (aliases are resolved in nr-executors).
pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(KimchiExecutor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_reasoning_content_long() {
        let mut body = serde_json::json!({
            "messages": [
                { "role": "user", "content": "solve x+5=12" },
                { "role": "assistant", "content": "x = 7", "reasoning_content": "x".repeat(100) }
            ]
        });
        strip_reasoning_content(&mut body);
        let msg = &body["messages"][1];
        assert_eq!(msg.get("reasoning_content"), None);
        assert_eq!(msg["content"].as_str(), Some("x = 7"));
    }

    #[test]
    fn test_strip_reasoning_content_preserves_placeholder() {
        let mut body = serde_json::json!({
            "messages": [
                { "role": "assistant", "content": "hello", "reasoning_content": " " }
            ]
        });
        strip_reasoning_content(&mut body);
        assert_eq!(body["messages"][0]["reasoning_content"].as_str(), Some(" "));
    }

    #[test]
    fn test_system_to_text_string() {
        let system = Json::String("be helpful".into());
        let text = system_to_text(&system);
        assert_eq!(text, "be helpful");
    }

    #[test]
    fn test_system_to_text_array() {
        let system = serde_json::json!([
            "part1",
            { "text": "part2" }
        ]);
        let text = system_to_text(&system);
        assert_eq!(text, "part1\npart2");
    }

    #[test]
    fn test_is_anthropic_backed_claude() {
        assert!(is_anthropic_backed_kimchi_model("claude-3-opus"));
        assert!(is_anthropic_backed_kimchi_model("Claude-2"));
        assert!(is_anthropic_backed_kimchi_model("ANTHROPIC"));
        assert!(!is_anthropic_backed_kimchi_model("gpt-4"));
    }

    #[test]
    fn test_merge_top_level_system_new() {
        let mut body = serde_json::json!({
            "system": "be helpful",
            "messages": [
                { "role": "user", "content": "hi" }
            ]
        });
        merge_top_level_system(&mut body);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"].as_str(), Some("system"));
        assert_eq!(msgs[0]["content"].as_str(), Some("be helpful"));
        assert_eq!(msgs[1]["role"].as_str(), Some("user"));
    }

    #[test]
    fn test_merge_top_level_system_merge_existing() {
        let mut body = serde_json::json!({
            "system": "helpful",
            "messages": [
                { "role": "system", "content": "existing" },
                { "role": "user", "content": "hi" }
            ]
        });
        merge_top_level_system(&mut body);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"].as_str(), Some("system"));
        assert!(msgs[0]["content"].as_str().unwrap().contains("helpful"));
        assert!(msgs[0]["content"].as_str().unwrap().contains("existing"));
    }

    #[test]
    fn test_strip_message_artifacts() {
        let mut body = serde_json::json!({
            "messages": [
                { "role": "user", "content": "hi", "cache_control": { "type": "ephemeral" } },
                {
                    "role": "assistant",
                    "content": [
                        { "type": "text", "text": "hello", "cache_control": { "type": "ephemeral" } }
                    ]
                }
            ]
        });
        strip_message_artifacts(&mut body);
        assert_eq!(body["messages"][0].get("cache_control"), None);
        assert_eq!(body["messages"][1]["content"][0].get("cache_control"), None);
    }
}
