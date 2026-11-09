//! Port of open-sse/executors/codex.js + utils/codexToolSchema.js.
//! Codex Responses API executor with tool schema sanitization and request normalization.
//! Remote image URLs are inlined as base64 before the request: the Codex
//! backend cannot fetch them itself (see `prefetch_images`).

use std::sync::Arc;
use std::sync::LazyLock;
use async_trait::async_trait;
use nr_core::{Credentials, ExecRequest, ExecResult, Executor, Json, Result};
use nr_core::http::HeaderList;
use nr_exec_base::base::{base_execute, BaseHooks};

static EMPTY: LazyLock<Json> = LazyLock::new(|| Json::Object(Default::default()));

/// Strip Unicode property escapes (\p{...}) from JSON schema patterns (Codex limitation).
fn has_unicode_property_escape(pattern: &str) -> bool {
    regex::Regex::new(r#"(^|[^\\])(\\\\)*\\[pP]\{"#).is_ok_and(|re| re.is_match(pattern))
}

fn strip_unicode_patterns(schema: &Json, stats: &mut usize) -> Json {
    match schema {
        Json::Object(map) => {
            let mut changed = false;
            let mut result = map.clone();

            // Strip `pattern` fields with \p{...}
            if let Some(Json::String(s)) = result.get("pattern") {
                if has_unicode_property_escape(s) {
                    *stats += 1;
                    result.remove("pattern");
                    changed = true;
                }
            }

            // Recurse into properties (keys are property names, not schema keywords)
            if let Some(Json::Object(props)) = result.get("properties") {
                let mut new_props = props.clone();
                let mut props_changed = false;
                for v in new_props.values_mut() {
                    let cleaned = strip_unicode_patterns(v, stats);
                    if cleaned != *v {
                        props_changed = true;
                        *v = cleaned;
                    }
                }
                if props_changed {
                    result.insert("properties".into(), Json::Object(new_props));
                    changed = true;
                }
            }

            // Recurse into other fields
            for v in result.values_mut() {
                let cleaned = strip_unicode_patterns(v, stats);
                if cleaned != *v {
                    changed = true;
                    *v = cleaned;
                }
            }
            if changed { Json::Object(result) } else { schema.clone() }
        }
        Json::Array(arr) => {
            let mut result = Vec::with_capacity(arr.len());
            let mut changed = false;
            for item in arr {
                let cleaned = strip_unicode_patterns(item, stats);
                if cleaned != *item {
                    changed = true;
                }
                result.push(cleaned);
            }
            if changed { Json::Array(result) } else { schema.clone() }
        }
        _ => schema.clone(),
    }
}

pub struct CodexExecutor {
    config: &'static Json,
}

impl CodexExecutor {
    pub fn new() -> Self {
        let config = nr_data::provider("codex").or_else(|| nr_data::provider("openai")).unwrap_or(&EMPTY);
        Self { config }
    }
}

#[async_trait]
impl BaseHooks for CodexExecutor {
    fn provider(&self) -> &str {
        "codex"
    }

    fn config(&self) -> &Json {
        self.config
    }

    fn build_headers(&self, creds: &Credentials, stream: bool, _url: &str, _model: &str, body: &Json) -> Result<HeaderList> {
        let mut headers = nr_exec_base::base::default_build_headers(self, creds, stream);

        // Session ID from body fields (stable across conversation)
        let session_id = body.get("session_id")
            .or_else(|| body.get("prompt_cache_key"))
            .or_else(|| body.get("conversation_id"))
            .and_then(Json::as_str)
            .map(|s| s.to_string())
            .unwrap_or_else(|| creds.connection_id().unwrap_or("default").into());

        headers.insert("session_id".into(), session_id);
        headers.insert("originator".into(), "codex_cli_rs".into());

        // Account/workspace binding to prevent cross-account leaks
        let account_id = creds.psd_str("workspaceId")
            .or_else(|| creds.psd_str("chatgptAccountId"))
            .or_else(|| creds.psd_str("accountId"));
        if let Some(id) = account_id {
            headers.insert("ChatGPT-Account-ID".into(), id.into());
        }

        Ok(headers)
    }

    fn transform_request(&self, _model: &str, body: &Json, _stream: bool, _creds: &Credentials) -> Result<Json> {
        let mut result = body.clone();

        // Convert system → developer role (keeps in cacheable prefix)
        if let Some(Json::Array(input)) = result.get_mut("input") {
            for item in input.iter_mut() {
                if let Json::Object(obj) = item {
                    if obj.get("role").and_then(Json::as_str) == Some("system")
                        && (obj.get("type").is_none() || obj.get("type").and_then(Json::as_str) == Some("message")) {
                        obj.insert("role".into(), Json::String("developer".into()));
                    }
                }
            }
        }

        // Strip server-generated item IDs (backend can't resolve with store=false)
        if let Some(Json::Array(input)) = result.get_mut("input") {
            let re = regex::Regex::new(r#"^(rs|fc|resp|msg)_"#).expect("static regex");
            input.retain(|item| {
                match item {
                    Json::String(s) => !re.is_match(s),
                    Json::Object(obj) => obj.get("type").and_then(Json::as_str) != Some("item_reference"),
                    _ => true,
                }
            });
        }

        // Strip Unicode patterns from tool schemas (Codex validator rejects \p{...})
        if let Some(Json::Array(tools)) = result.get_mut("tools") {
            for tool in tools.iter_mut() {
                if let Json::Object(t) = tool {
                    if let Some(params) = t.get_mut("parameters") {
                        let mut stats = 0;
                        *params = strip_unicode_patterns(params, &mut stats);
                    }
                }
            }
        }

        // Force streaming (Codex API requires it)
        result["stream"] = Json::Bool(true);

        // Remove unsupported parameters
        for key in &["temperature", "top_p", "frequency_penalty", "presence_penalty",
                     "logprobs", "top_logprobs", "n", "seed", "max_tokens",
                     "max_completion_tokens", "max_output_tokens", "user",
                     "prompt_cache_retention", "metadata", "stream_options",
                     "safety_identifier", "previous_response_id"] {
            if let Some(obj) = result.as_object_mut() {
                obj.remove(*key);
            }
        }

        Ok(result)
    }
}

#[async_trait]
impl Executor for CodexExecutor {
    fn provider(&self) -> &str {
        "codex"
    }

    fn no_auth(&self) -> bool {
        BaseHooks::no_auth(self)
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        // The Codex backend cannot fetch remote images, so any `image_url` part
        // is inlined as a data URI before the request leaves this process.
        let body = prefetch_images(req.body.clone()).await;
        base_execute(self, ExecRequest { body, ..req }).await
    }
}

/// `prefetchImages`: rewrite `image_url` parts into `input_image` parts,
/// inlining remote URLs as base64 data URIs. Unchanged parts pass through.
async fn prefetch_images(body: Json) -> Json {
    let mut body = body;
    let Some(items) = body.get_mut("input").and_then(Json::as_array_mut) else {
        return body;
    };

    for item in items.iter_mut() {
        let Some(content) = item.get_mut("content").and_then(Json::as_array_mut) else {
            continue;
        };
        let mut rewritten = Vec::with_capacity(content.len());
        for part in content.iter() {
            if part.get("type").and_then(Json::as_str) != Some("image_url") {
                rewritten.push(part.clone());
                continue;
            }
            let image_url = part.get("image_url").cloned().unwrap_or(Json::Null);
            let url = match &image_url {
                Json::String(s) => s.clone(),
                other => other.get("url").and_then(Json::as_str).unwrap_or("").to_string(),
            };
            let detail = image_url
                .get("detail")
                .and_then(Json::as_str)
                .unwrap_or("auto")
                .to_string();
            let resolved = if url.is_empty() {
                url
            } else if url.starts_with("data:") {
                url
            } else {
                let options = nr_translator::concerns::image::FetchOptions {
                    timeout_ms: 15_000,
                    ..Default::default()
                };
                match nr_translator::concerns::image::fetch_image_as_base64(&url, &options).await {
                    Some(fetched) => fetched.url,
                    None => url,
                }
            };
            rewritten.push(serde_json::json!({
                "type": "input_image",
                "image_url": resolved,
                "detail": detail,
            }));
        }
        if let Some(slot) = item.get_mut("content") {
            *slot = Json::Array(rewritten);
        }
    }
    body
}

pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(CodexExecutor::new())
}

#[cfg(test)]
mod prefetch_tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn data_uri_images_become_input_image() {
        let body = json!({"input": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "look"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAA", "detail": "high"}},
            ],
        }]});
        let out = prefetch_images(body).await;
        let parts = out["input"][0]["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "input_image");
        assert_eq!(parts[1]["image_url"], "data:image/png;base64,AAA");
        assert_eq!(parts[1]["detail"], "high");
    }

    #[tokio::test]
    async fn detail_defaults_to_auto() {
        let body = json!({"input": [{"content": [
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAA"}},
        ]}]});
        let out = prefetch_images(body).await;
        assert_eq!(out["input"][0]["content"][0]["detail"], "auto");
    }

    #[tokio::test]
    async fn unreachable_remote_url_keeps_the_original_url() {
        // An unroutable host: the fetch fails and the URL is passed through,
        // matching `fetched?.url || url`.
        let body = json!({"input": [{"content": [
            {"type": "image_url", "image_url": {"url": "http://127.0.0.1:9/x.png"}},
        ]}]});
        let out = prefetch_images(body).await;
        assert_eq!(out["input"][0]["content"][0]["type"], "input_image");
        assert_eq!(out["input"][0]["content"][0]["image_url"], "http://127.0.0.1:9/x.png");
    }

    #[tokio::test]
    async fn bodies_without_input_are_untouched() {
        let body = json!({"messages": [{"role": "user", "content": "hi"}]});
        assert_eq!(prefetch_images(body.clone()).await, body);
    }

    #[tokio::test]
    async fn string_image_url_shape_is_accepted() {
        let body = json!({"input": [{"content": [
            {"type": "image_url", "image_url": "data:image/png;base64,BBB"},
        ]}]});
        let out = prefetch_images(body).await;
        assert_eq!(out["input"][0]["content"][0]["image_url"], "data:image/png;base64,BBB");
    }
}
