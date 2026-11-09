//! Source/target format detection + request/response translation glue.
//! Port of the chatCore translation block (open-sse/handlers/chatCore.js) and
//! open-sse/translator/formats.js `detectFormatByEndpoint`.

use nr_common::provider::{detect_format, get_target_format};
use nr_core::{Credentials, Json};
use nr_translator::registry::{translate_request, translate_response, TranslationOptions};
use nr_translator::state::StreamState;

/// Detect the source (client) wire format from the request path + body.
/// Port of `detectFormatByEndpoint`; `None` falls back to body detection.
pub fn detect_format_by_endpoint(pathname: &str, body: &Json) -> &'static str {
    if pathname.contains("/v1/responses") {
        return "openai-responses";
    }
    if pathname.contains("/v1/messages") {
        return "claude";
    }
    // Cursor CLI sends a Responses body through the chat endpoint.
    if pathname.contains("/v1/chat/completions") && body.get("input").is_some_and(|v| v.is_array()) {
        return "openai";
    }
    detect_format(body)
}

/// Target (provider) wire format for a provider + credentials.
pub fn target_format_for(provider: &str, credentials: &Credentials) -> &'static str {
    let value = credentials.clone().into_value();
    get_target_format(provider, Some(&value))
}

/// Translate a client-format body into the provider format.
pub fn translate_request_to_provider(
    source_format: &str,
    target_format: &str,
    model: &str,
    body: Json,
    stream: bool,
    credentials: Option<&mut Credentials>,
) -> Json {
    translate_request_to_provider_for(source_format, target_format, model, body, stream, credentials, None)
}

/// Same as [`translate_request_to_provider`], but resolves the model's `strip[]`
/// list (image/audio parts the upstream rejects) when `provider` is known.
pub fn translate_request_to_provider_for(
    source_format: &str,
    target_format: &str,
    model: &str,
    body: Json,
    stream: bool,
    credentials: Option<&mut Credentials>,
    provider: Option<&str>,
) -> Json {
    let alias_or_id = provider.unwrap_or("");
    let options = TranslationOptions {
        // `getModelStrip(alias, modelId)` — empty when the entry has no strip[].
        strip_content_types: if alias_or_id.is_empty() {
            Vec::new()
        } else {
            nr_data::get_model_strip(alias_or_id, model)
        },
        ..TranslationOptions::default()
    };
    translate_request(source_format, target_format, model, body, stream, credentials, &options)
}

/// Translate one upstream response chunk into client-format chunks.
/// `chunk == None` is the end-of-stream flush call.
pub fn translate_chunk_to_client(
    target_format: &str,
    source_format: &str,
    chunk: Option<&Json>,
    state: &mut StreamState,
) -> Vec<Json> {
    translate_response(target_format, source_format, chunk, state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn endpoint_detection() {
        assert_eq!(detect_format_by_endpoint("/v1/responses", &json!({})), "openai-responses");
        assert_eq!(detect_format_by_endpoint("/api/v1/messages", &json!({})), "claude");
        assert_eq!(
            detect_format_by_endpoint("/v1/chat/completions", &json!({"input": []})),
            "openai"
        );
        assert_eq!(
            detect_format_by_endpoint("/v1/chat/completions", &json!({"messages": []})),
            "openai"
        );
    }

    #[test]
    fn body_detection_fallback() {
        // Gemini contents array → gemini
        assert_eq!(detect_format_by_endpoint("/v1/chat/completions", &json!({"contents": []})), "gemini");
    }

    #[test]
    fn target_format_anthropic_is_claude() {
        let c = Credentials::from_value(json!({}));
        assert_eq!(target_format_for("anthropic", &c), "claude");
        assert_eq!(target_format_for("gemini", &c), "gemini");
        assert_eq!(target_format_for("openai", &c), "openai");
    }

    #[test]
    fn target_format_openai_compatible_uses_api_type() {
        let c = Credentials::from_value(json!({"providerSpecificData": {"apiType": "responses"}}));
        assert_eq!(target_format_for("openai-compatible-chat-uuid", &c), "openai-responses");
        let c2 = Credentials::from_value(json!({"providerSpecificData": {"apiType": "chat"}}));
        assert_eq!(target_format_for("openai-compatible-chat-uuid", &c2), "openai");
    }
}
