//! Strip multimodal content blocks a model cannot read. Port of open-sse/translator/concerns/modality.js.
//! TODO(dedupe): full implementation with format-specific block filtering and capabilities checking

use nr_core::Json;

/// Remove media blocks the model can't read, in-place on the source-format body.
/// TODO: full implementation with capability checking and block filtering per format
pub fn strip_unsupported_modalities(body: &mut Json, _source_format: &str, _caps: &Json) -> bool {
    if !body.is_object() {
        return false;
    }

    // Stub: full implementation needed
    // - OpenAI format: filter messages[].content[] for image_url, input_audio, file blocks
    // - Claude format: filter messages[].content[] for image, document blocks
    // - Gemini format: filter contents[].parts[] by mime type
    // - etc.

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_unsupported_modalities_noop() {
        let mut body = serde_json::json!({
            "messages": [
                {"role": "user", "content": "hello"}
            ]
        });
        let caps = serde_json::json!({
            "vision": true,
            "audioInput": true,
            "pdf": true
        });
        let changed = strip_unsupported_modalities(&mut body, "openai", &caps);
        assert!(!changed);
    }
}
