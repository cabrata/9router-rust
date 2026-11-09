//! Pre-fetch remote image URLs into base64. Port of open-sse/translator/concerns/prefetch.js.
//! TODO(dedupe): async image fetching with SSRF protection and base64 conversion

use nr_core::Json;

/// Replace remote image URLs with base64 data when the target needs inline data.
/// No-op when target accepts remote URLs (e.g. openai, claude) or body has none.
/// Returns count of images converted.
/// TODO: full implementation with async fetch and image format conversion
pub async fn prefetch_remote_images(body: &mut Json, _source_format: &str, _target_format: &str, _options: &Json) -> usize {
    if !body.is_object() {
        return 0;
    }

    // Stub: full implementation needed
    // - Collect remote image URL references based on format
    // - For each URL, fetch and convert to base64
    // - Replace URL with base64 data URI
    // - Return count of conversions

    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_prefetch_remote_images_noop() {
        let mut body = serde_json::json!({
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hello"}]}
            ]
        });
        let options = serde_json::json!({});
        let count = prefetch_remote_images(&mut body, "openai", "openai", &options).await;
        assert_eq!(count, 0);
    }
}
