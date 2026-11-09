//! Port of open-sse/executors/zed.js.
//!
//! Routes requests to Zed's hosted LLM aggregator (cloud.zed.dev/completions),
//! a multi-format proxy fronting Anthropic/OpenAI/Google/xAI depending on the
//! requested model. Wire protocol: POST /completions with NDJSON/SSE response,
//! authenticated with a short-lived LLM bearer token exchanged from the
//! RSA-decrypted access_token.
//!
//! Overrides execute() entirely (does NOT use DefaultExecutor's pipeline)
//! because the Zed wire shape (thread envelope, LLM-token exchange, NDJSON
//! status frames) doesn't fit the generic transformRequest/buildUrl contract.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use indexmap::IndexMap;
use nr_core::http::UpstreamResponse;
use nr_core::{ExecRequest, ExecResult, Executor, Json, JsonMap, Result};
use nr_translator::{translate_request, translate_response, init_state, TranslationOptions, formats as tr_formats};
use serde_json::json;

use crate::zed_auth::{
    build_zed_user_auth_header, resolve_zed_models, ZED_CLOUD_BASE_URL, ZED_HEADERS,
    ZED_LLM_BASE_URL,
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

/// Build the provider-specific request from OpenAI format using translators.
fn build_provider_request(
    provider: &str,
    model: &str,
    body: &Json,
    stream: bool,
    _credentials: &nr_core::Credentials,
) -> Json {
    let options = TranslationOptions::default();
    let body_copy = body.clone();

    match provider {
        ZED_PROVIDER_ANTHROPIC => {
            // Translate OpenAI format to Claude format
            translate_request(
                tr_formats::OPENAI,
                tr_formats::CLAUDE,
                model,
                body_copy,
                stream,
                None,
                &options,
            )
        }
        ZED_PROVIDER_GOOGLE => {
            // Translate OpenAI format to Gemini format
            let mut translated = translate_request(
                tr_formats::OPENAI,
                tr_formats::GEMINI,
                model,
                body_copy,
                stream,
                None,
                &options,
            );

            // Drop safetySettings for Zed Google path (Vertex vocabulary)
            if let Json::Object(ref mut obj) = translated {
                obj.remove("safetySettings");
            }
            translated
        }
        ZED_PROVIDER_OPENAI => {
            // Translate OpenAI format to OpenAI Responses format
            translate_request(
                tr_formats::OPENAI,
                tr_formats::OPENAI_RESPONSES,
                model,
                body_copy,
                stream,
                None,
                &options,
            )
        }
        // xAI is OpenAI-shaped — forward as-is
        _ => body_copy,
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
        text = &text[5..].trim_start();
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
            .unwrap_or_else(|| {
                // Fallback to empty config
                Box::leak(Box::new(Json::Object(JsonMap::new())))
            });
        Self {
            provider: provider.to_string(),
            config,
        }
    }

    /// Map provider output format to internal format for translation
    fn provider_to_format(provider: &str) -> &'static str {
        match provider {
            ZED_PROVIDER_ANTHROPIC => tr_formats::CLAUDE,
            ZED_PROVIDER_GOOGLE => tr_formats::GEMINI,
            ZED_PROVIDER_OPENAI => tr_formats::OPENAI_RESPONSES,
            _ => tr_formats::OPENAI,
        }
    }

    /// Fetch the Zed model catalog and resolve the provider for the requested model.
    async fn resolve_model(&self, model: &str, credentials: &nr_core::Credentials) -> Result<&'static str> {
        let base_url = self
            .config
            .get("llmBaseUrl")
            .and_then(|v| v.as_str())
            .unwrap_or(ZED_LLM_BASE_URL);

        match resolve_zed_models(credentials, base_url, false, None).await {
            Ok(Some(catalog)) => {
                if let Some(raw_models) = catalog.get("rawModels").and_then(|v| v.as_array()) {
                    for raw in raw_models {
                        if let Some(raw_id) = raw.get("id").and_then(|v| v.as_str()) {
                            if raw_id == model {
                                let provider = raw
                                    .get("provider")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                return Ok(normalize_zed_provider(Some(provider), model));
                            }
                        }
                    }
                }
                // Not found in cache, try refresh
                match resolve_zed_models(credentials, base_url, true, None).await {
                    Ok(Some(catalog)) => {
                        if let Some(raw_models) = catalog.get("rawModels").and_then(|v| v.as_array()) {
                            for raw in raw_models {
                                if let Some(raw_id) = raw.get("id").and_then(|v| v.as_str()) {
                                    if raw_id == model {
                                        let provider = raw
                                            .get("provider")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("");
                                        return Ok(normalize_zed_provider(Some(provider), model));
                                    }
                                }
                            }
                        }
                        Ok(normalize_zed_provider(None, model))
                    }
                    _ => Ok(normalize_zed_provider(None, model)),
                }
            }
            Err(_e) => {
                // Model catalog unavailable, infer provider from model name
                Ok(normalize_zed_provider(None, model))
            }
            Ok(None) => Ok(normalize_zed_provider(None, model)),
        }
    }
}

impl nr_exec_base::base::BaseHooks for ZedExecutor {
    fn provider(&self) -> &str {
        "zed"
    }

    fn config(&self) -> &nr_core::Json {
        self.config
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

    async fn execute(&self, mut req: ExecRequest<'_>) -> Result<ExecResult> {
        let provider = self.resolve_model(&req.model, &req.credentials).await?;

        let provider_request = build_provider_request(provider, &req.model, &req.body, req.stream, &req.credentials);

        let body_record = req.body.as_object().cloned().unwrap_or_default();
        let thread_id = body_record
            .get("thread_id")
            .and_then(|v| v.as_str())
            .or_else(|| req.credentials.str("_clientSessionId"))
            .map(|s| s.to_string());

        let prompt_id = body_record
            .get("prompt_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let payload = json!({
            "thread_id": thread_id,
            "prompt_id": prompt_id,
            "provider": provider,
            "model": &req.model,
            "provider_request": provider_request,
        });

        let base_url = self
            .config
            .get("llmBaseUrl")
            .and_then(|v| v.as_str())
            .unwrap_or(ZED_LLM_BASE_URL);

        let client = reqwest::Client::new();
        let auth_header = build_zed_user_auth_header(&req.credentials)?;
        let url = format!("{}/completions", base_url.trim_end_matches('/'));

        let app_version = self
            .config
            .get("appVersion")
            .and_then(|v| v.as_str())
            .unwrap_or("0.200.0");

        let req_builder = client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/x-ndjson, text/event-stream, */*")
            .header("User-Agent", "9router/zed")
            .header("x-zed-version", app_version)
            .header(ZED_HEADERS.client_supports_status, "true")
            .header(ZED_HEADERS.client_supports_stream_ended, "true")
            .header("Authorization", auth_header);

        let response = req_builder
            .json(&payload)
            .send()
            .await
            .map_err(|e| nr_core::Error::other(format!("Zed request failed: {}", e)))?;

        // TODO: Wrap response with format translation
        // The response contains NDJSON/SSE from Zed's upstream provider (Claude/Gemini/OpenAI/xAI).
        // We need to:
        // 1. Parse each line as a Zed envelope ({"event": <chunk>} or {"status": ...} or [DONE])
        // 2. Extract the event/chunk and convert from provider format back to OpenAI
        // 3. Re-wrap each chunk in SSE format for the client
        //
        // This requires a streaming response wrapper that transforms the body on-the-fly.
        // For now, the response is passed through; translation happens via response_format.
        let upstream = UpstreamResponse::from_reqwest(response);

        let mut headers = IndexMap::new();
        headers.insert("Content-Type".to_string(), "application/json".to_string());

        // Tell downstream code what format to translate FROM
        // shortcut: No streaming wrapper means chunks arrive provider-shaped; we rely on
        // downstream translate_response to convert them back. The format MUST match what
        // Zed's upstream actually sends (Claude/Gemini/OpenAI/xAI format).
        let response_format = Some(Self::provider_to_format(provider).to_string());

        Ok(ExecResult {
            response: upstream,
            url: url.clone(),
            headers,
            transformed_body: payload,
            response_format,
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
    fn normalize_zed_provider_anthropic() {
        assert_eq!(normalize_zed_provider(Some("anthropic"), "any-model"), ZED_PROVIDER_ANTHROPIC);
        // Provider value is not "anthropic", so fall back to model inference
        assert_eq!(normalize_zed_provider(Some("other"), "claude-3-opus"), ZED_PROVIDER_ANTHROPIC);
        assert_eq!(normalize_zed_provider(None, "claude-3-opus"), ZED_PROVIDER_ANTHROPIC);
    }

    #[test]
    fn normalize_zed_provider_google() {
        assert_eq!(normalize_zed_provider(Some("google"), "any-model"), ZED_PROVIDER_GOOGLE);
        assert_eq!(normalize_zed_provider(Some("gemini"), "any-model"), ZED_PROVIDER_GOOGLE);
        assert_eq!(normalize_zed_provider(None, "gemini-2.0-flash"), ZED_PROVIDER_GOOGLE);
    }

    #[test]
    fn normalize_zed_provider_openai() {
        assert_eq!(normalize_zed_provider(Some("open_ai"), "any-model"), ZED_PROVIDER_OPENAI);
        assert_eq!(normalize_zed_provider(Some("openai"), "any-model"), ZED_PROVIDER_OPENAI);
        assert_eq!(normalize_zed_provider(None, "gpt-4"), ZED_PROVIDER_OPENAI);
    }

    #[test]
    fn normalize_zed_provider_xai() {
        assert_eq!(normalize_zed_provider(Some("xai"), "any-model"), ZED_PROVIDER_XAI);
        assert_eq!(normalize_zed_provider(Some("x_ai"), "any-model"), ZED_PROVIDER_XAI);
        assert_eq!(normalize_zed_provider(None, "grok-2"), ZED_PROVIDER_XAI);
    }

    #[test]
    fn unwrap_zed_line_done() {
        let payload = unwrap_zed_line("[DONE]");
        assert!(matches!(payload, Some(ZedPayload::Done)));
    }

    #[test]
    fn unwrap_zed_line_with_data_prefix() {
        let line = r#"data: {"event": "test"}"#;
        let payload = unwrap_zed_line(line);
        assert!(matches!(payload, Some(ZedPayload::Event(_))));
    }

    #[test]
    fn zed_executor_creates() {
        let executor = ZedExecutor::new("zed");
        assert_eq!(executor.provider(), "zed");
    }
}
