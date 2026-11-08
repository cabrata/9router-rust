//! Wire-format identifiers. Port of open-sse/translator/formats.js.
//!
//! Formats stay plain strings, as in JS: the provider registry also carries
//! formats outside this list (e.g. "grok-web", "perplexity-web").

pub const OPENAI: &str = "openai";
pub const OPENAI_RESPONSES: &str = "openai-responses";
pub const OPENAI_RESPONSE: &str = "openai-response";
pub const CLAUDE: &str = "claude";
pub const GEMINI: &str = "gemini";
pub const GEMINI_CLI: &str = "gemini-cli";
pub const VERTEX: &str = "vertex";
pub const CODEX: &str = "codex";
pub const ANTIGRAVITY: &str = "antigravity";
pub const KIRO: &str = "kiro";
pub const CURSOR: &str = "cursor";
pub const OLLAMA: &str = "ollama";
pub const COMMANDCODE: &str = "commandcode";

/// Source format implied by the request path; `None` falls back to body-based
/// detection. Port of `detectFormatByEndpoint`.
pub fn detect_format_by_endpoint(pathname: &str, body: &crate::Json) -> Option<&'static str> {
    if pathname.contains("/v1/responses") {
        return Some(OPENAI_RESPONSES);
    }
    if pathname.contains("/v1/messages") {
        return Some(CLAUDE);
    }
    // Cursor CLI sends a Responses body through the chat endpoint.
    if pathname.contains("/v1/chat/completions") && body.get("input").is_some_and(|v| v.is_array()) {
        return Some(OPENAI);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn endpoint_detection() {
        assert_eq!(detect_format_by_endpoint("/v1/responses", &json!({})), Some(OPENAI_RESPONSES));
        assert_eq!(detect_format_by_endpoint("/api/v1/messages", &json!({})), Some(CLAUDE));
        assert_eq!(detect_format_by_endpoint("/v1/chat/completions", &json!({"input": []})), Some(OPENAI));
        assert_eq!(detect_format_by_endpoint("/v1/chat/completions", &json!({"messages": []})), None);
    }
}
