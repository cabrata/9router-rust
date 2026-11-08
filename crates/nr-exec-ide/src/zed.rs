//! Port of open-sse/executors/zed.js.
//!
//! Routes requests to Zed's hosted LLM aggregator (cloud.zed.dev/completions),
//! a multi-format proxy fronting Anthropic/OpenAI/Google/xAI depending on the
//! requested model. Wire protocol: POST /completions with NDJSON/SSE response,
//! authenticated with a short-lived LLM bearer token exchanged from the
//! RSA-decrypted access_token (see shared/zedAuth.rs).
//!
//! Overrides execute() entirely (does NOT use DefaultExecutor's pipeline).

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use nr_core::http::HeaderList;
use nr_core::{Credentials, ExecRequest, ExecResult, Executor, Json, JsonMap, Result};
use serde_json::json;

use crate::zed_auth::{
    build_zed_user_auth_header, decrypt_zed_access_token, encode_zed_private_key_verifier,
    fetch_zed_llm_token, resolve_zed_models, ZED_CLOUD_BASE_URL, ZED_HEADERS, ZED_LLM_BASE_URL,
};

/// Wire values for the `provider` field of POST /completions.
/// Must match Zed's catalog spellings exactly.
const ZED_PROVIDER_ANTHROPIC: &str = "anthropic";
const ZED_PROVIDER_OPENAI: &str = "open_ai";
const ZED_PROVIDER_GOOGLE: &str = "google";
const ZED_PROVIDER_XAI: &str = "x_ai";

/// Normalize provider value from config or infer from model name.
fn normalize_zed_provider(value: Option<&str>, model: &str) -> &'static str {
    if let Some(v) = value {
        let v_lower = v.to_lowercase();
        if v_lower == "anthropic" {
            return ZED_PROVIDER_ANTHROPIC;
        }
        if v_lower == "openai" || v_lower == "open_ai" {
            return ZED_PROVIDER_OPENAI;
        }
        if v_lower == "google" || v_lower == "gemini" {
            return ZED_PROVIDER_GOOGLE;
        }
        if v_lower == "xai" || v_lower == "x_ai" || v_lower == "x-ai" {
            return ZED_PROVIDER_XAI;
        }
    }

    let m_lower = model.to_lowercase();
    if m_lower.contains("claude") {
        return ZED_PROVIDER_ANTHROPIC;
    }
    if m_lower.contains("gemini") {
        return ZED_PROVIDER_GOOGLE;
    }
    if m_lower.contains("grok") || m_lower.contains("xai") {
        return ZED_PROVIDER_XAI;
    }
    ZED_PROVIDER_OPENAI
}

/// Build the provider-specific request from OpenAI format.
/// TODO(dedupe): use translators from nr_translator once they expose init/request fns.
fn build_provider_request(
    provider: &str,
    model: &str,
    body: &Json,
    stream: bool,
    _credentials: &Credentials,
) -> Json {
    let mut body_obj = body.as_object().cloned().unwrap_or_default();
    body_obj.insert("model".into(), Json::String(model.to_string()));
    body_obj.insert("stream".into(), Json::Bool(stream));

    match provider {
        ZED_PROVIDER_ANTHROPIC => {
            // TODO(dedupe): call openaiToClaudeRequest translator
            body_obj
        }
        ZED_PROVIDER_GOOGLE => {
            // TODO(dedupe): call openaiToGeminiRequest translator
            // Drop safetySettings for Zed Google path (Vertex vocabulary)
            body_obj.remove("safetySettings");
            body_obj
        }
        ZED_PROVIDER_OPENAI => {
            // TODO(dedupe): call openaiToOpenAIResponsesRequest translator
            body_obj
        }
        // xAI is OpenAI-shaped — forward as-is
        _ => body_obj,
    }
}

/// Create an error chunk in OpenAI format.
fn create_error_chunk(model: &str, message: &str) -> Json {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    json!({
        "id": format!("chatcmpl-zed-error-{}", now * 1000),
        "object": "chat.completion.chunk",
        "created": now,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": { "content": format!("[Zed error] {}", message) },
            "finish_reason": "stop"
        }]
    })
}

/// Parse a line from Zed's NDJSON/SSE response.
#[derive(Debug, Clone)]
enum ZedPayload {
    Done,
    Event(Json),
    Status(Json),
}

fn unwrap_zed_line(line: &str) -> Option<ZedPayload> {
    let mut text = line.trim_end_matches('\r').trim();

    if text.is_empty() {
        return None;
    }

    if text.starts_with("data:") {
        text = text[5..].trim_start();
    }

    if text == "[DONE]" {
        return Some(ZedPayload::Done);
    }

    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(parsed) => {
            if let Some(obj) = parsed.as_object() {
                if obj.contains_key("event") {
                    return Some(ZedPayload::Event(parsed));
                }
                if obj.contains_key("status") {
                    return Some(ZedPayload::Status(parsed));
                }
            }
            Some(ZedPayload::Event(parsed))
        }
        Err(_) => None,
    }
}

/// Normalize Zed status object into a standard form.
fn normalize_status(status: &Json) -> Option<JsonMap> {
    let mut result = JsonMap::new();

    if let Some(s) = status.as_str() {
        result.insert("type".into(), Json::String(s.to_string()));
        return Some(result);
    }

    if let Some(obj) = status.as_object() {
        if let Some((key, val)) = obj.iter().next() {
            result.insert("type".into(), Json::String(key.clone()));
            if let Some(val_obj) = val.as_object() {
                result.extend(val_obj.clone());
            }
            return Some(result);
        }
        return Some(obj.clone());
    }

    None
}

/// ZedExecutor routes requests through Zed's aggregator with format translation.
pub struct ZedExecutor {
    provider: String,
    config: &'static Json,
}

impl ZedExecutor {
    pub fn new(provider: &str) -> Self {
        let config = nr_data::provider(provider)
            .or_else(|| nr_data::provider("zed"))
            .unwrap_or_else(|| &Json::Object(JsonMap::new()));
        Self {
            provider: provider.to_string(),
            config,
        }
    }

    /// Fetch the Zed model catalog and resolve the provider for the requested model.
    async fn resolve_model(
        &self,
        model: &str,
        credentials: &Credentials,
    ) -> (Option<Json>, &'static str) {
        match fetch_zed_llm_token(credentials).await {
            Ok(_) => {
                // TODO(dedupe): call resolveZedModels to get catalog
                // For now, infer provider from model name
                let provider = normalize_zed_provider(None, model);
                (None, provider)
            }
            Err(_) => {
                // If token fetch fails, infer provider from model name
                let provider = normalize_zed_provider(None, model);
                (None, provider)
            }
        }
    }
}

impl nr_exec_base::base::BaseHooks for ZedExecutor {
    fn provider(&self) -> &str {
        "zed"
    }

    fn config(&self) -> &Json {
        self.config
    }

    fn base_urls(&self) -> Vec<String> {
        vec![ZED_LLM_BASE_URL.to_string()]
    }
}

#[async_trait]
impl Executor for ZedExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn no_auth(&self) -> bool {
        false
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        let (model, body, stream, credentials, _signal, log, _proxy_options) = (
            req.model,
            req.body,
            req.stream,
            req.credentials,
            req.signal,
            req.log,
            req.proxy_options,
        );

        let (_raw_model, provider) = self.resolve_model(model, credentials).await;

        let provider_request = build_provider_request(provider, model, body, stream, credentials);

        // Build the Zed payload
        let body_record = body.as_object().cloned().unwrap_or_default();
        let thread_id = body_record
            .get("thread_id")
            .and_then(Json::as_str)
            .map(|s| s.to_string())
            .or_else(|| {
                credentials
                    .psd_str("_clientSessionId")
                    .map(|s| s.to_string())
            });

        let mut payload = json!({
            "provider": provider,
            "model": model,
            "provider_request": provider_request,
        });

        if let Some(thread_id) = thread_id {
            payload["thread_id"] = Json::String(thread_id);
        }

        if let Some(prompt_id) = body_record.get("prompt_id") {
            payload["prompt_id"] = prompt_id.clone();
        }

        // Fetch LLM token and make the request
        let token = fetch_zed_llm_token(credentials).await?;

        let mut headers = HeaderList::new();
        headers.insert("Content-Type".into(), "application/json".into());
        headers.insert("Accept".into(), "application/x-ndjson, text/event-stream, */*".into());
        headers.insert("User-Agent".into(), "9router/zed".into());
        headers.insert("x-zed-version".into(), "0.200.0".into());
        headers.insert(
            ZED_HEADERS.client_supports_status.into(),
            "true".into(),
        );
        headers.insert(
            ZED_HEADERS.client_supports_stream_ended.into(),
            "true".into(),
        );
        headers.insert("Authorization".into(), format!("Bearer {}", token));

        let url = format!("{}/completions", ZED_LLM_BASE_URL);
        let body_bytes = Bytes::from(serde_json::to_vec(&payload)?);

        let response = nr_core::http::proxy_fetch(
            nr_core::http::FetchRequest {
                url: url.clone(),
                method: "POST".into(),
                headers,
                body: Some(body_bytes),
            },
            req.proxy_options,
        )
        .await?;

        // TODO(dedupe): Wrap response stream with Zed NDJSON parsing and format translation.
        // This requires streaming support and translator integration not yet available.
        // For now, pass through the raw response.

        Ok(ExecResult {
            response,
            url,
            headers: HeaderList::from(vec![
                ("Content-Type".into(), "application/json".into()),
                ("Authorization".into(), "Bearer <zed-llm-token>".into()),
            ]),
            transformed_body: Some(payload),
        })
    }
}

pub fn create(provider: &str) -> Arc<dyn Executor> {
    Arc::new(ZedExecutor::new(provider))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_zed_provider_exact() {
        assert_eq!(normalize_zed_provider(Some("anthropic"), ""), ZED_PROVIDER_ANTHROPIC);
        assert_eq!(normalize_zed_provider(Some("open_ai"), ""), ZED_PROVIDER_OPENAI);
        assert_eq!(normalize_zed_provider(Some("google"), ""), ZED_PROVIDER_GOOGLE);
        assert_eq!(normalize_zed_provider(Some("x_ai"), ""), ZED_PROVIDER_XAI);
    }

    #[test]
    fn normalize_zed_provider_aliases() {
        assert_eq!(normalize_zed_provider(Some("openai"), ""), ZED_PROVIDER_OPENAI);
        assert_eq!(normalize_zed_provider(Some("gemini"), ""), ZED_PROVIDER_GOOGLE);
        assert_eq!(normalize_zed_provider(Some("x-ai"), ""), ZED_PROVIDER_XAI);
        assert_eq!(normalize_zed_provider(Some("xai"), ""), ZED_PROVIDER_XAI);
    }

    #[test]
    fn normalize_zed_provider_infer_model() {
        assert_eq!(normalize_zed_provider(None, "claude-3-opus"), ZED_PROVIDER_ANTHROPIC);
        assert_eq!(normalize_zed_provider(None, "gemini-pro"), ZED_PROVIDER_GOOGLE);
        assert_eq!(normalize_zed_provider(None, "grok-2"), ZED_PROVIDER_XAI);
        assert_eq!(normalize_zed_provider(None, "gpt-4"), ZED_PROVIDER_OPENAI);
    }

    #[test]
    fn unwrap_zed_line_done() {
        let payload = unwrap_zed_line("[DONE]");
        assert!(matches!(payload, Some(ZedPayload::Done)));
    }

    #[test]
    fn unwrap_zed_line_event() {
        let line = r#"{"event": {"choices": []}}"#;
        let payload = unwrap_zed_line(line);
        assert!(matches!(payload, Some(ZedPayload::Event(_))));
    }

    #[test]
    fn unwrap_zed_line_status() {
        let line = r#"{"status": {"type": "stream_ended"}}"#;
        let payload = unwrap_zed_line(line);
        assert!(matches!(payload, Some(ZedPayload::Status(_))));
    }

    #[test]
    fn unwrap_zed_line_empty() {
        assert_eq!(unwrap_zed_line(""), None);
        assert_eq!(unwrap_zed_line("   "), None);
    }

    #[test]
    fn create_error_chunk_format() {
        let chunk = create_error_chunk("gpt-4", "rate limit exceeded");
        assert!(chunk.get("object").and_then(Json::as_str) == Some("chat.completion.chunk"));
        assert!(chunk.get("model").and_then(Json::as_str) == Some("gpt-4"));
    }
}
