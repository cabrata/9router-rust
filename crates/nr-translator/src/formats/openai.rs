//! OpenAI format helpers. Port of open-sse/translator/formats/openai.js.
//! TODO(dedupe): full implementation with message filtering and tool normalization

use nr_core::Json;

/// Filter messages to OpenAI standard format
/// Remove: thinking, redacted_thinking, signature, and other non-OpenAI blocks
/// TODO: full implementation with content block filtering and tool normalization
pub fn filter_to_openai_format(body: &mut Json, _opts: &FilterOptions) -> &mut Json {
    if !body.get("messages").and_then(|v| v.as_array()).is_some() {
        return body;
    }

    // Stub: full implementation needed
    // - Normalize developer role to system
    // - Filter invalid content types
    // - Remove empty text blocks
    // - Normalize tools from Claude/Gemini to OpenAI format

    body
}

#[derive(Clone, Debug, Default)]
pub struct FilterOptions {
    pub preserve_cache_control: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_filter_to_openai_format_noop() {
        let mut body = json!({
            "messages": [
                {"role": "user", "content": "hello"}
            ]
        });
        let opts = FilterOptions::default();
        let result = filter_to_openai_format(&mut body, &opts);
        assert!(result.is_object());
    }
}
