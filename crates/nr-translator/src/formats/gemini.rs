//! Gemini format helpers. Port of open-sse/translator/formats/gemini.js (601 lines).
//! TODO(dedupe): full implementation with content part building and safety ratings

use nr_core::Json;

/// Build Gemini request body from OpenAI messages
/// TODO: full implementation with contents[] array building and safety settings
pub fn build_gemini_request(_body: &Json) -> Json {
    serde_json::json!({})
}

/// Parse Gemini response into OpenAI completion format
/// TODO: full implementation with content aggregation and finish_reason mapping
pub fn parse_gemini_response(_response: &Json) -> Json {
    serde_json::json!({})
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_build_gemini_request_basic() {
        let body = json!({"messages": []});
        let result = build_gemini_request(&body);
        assert!(result.is_object());
    }
}
