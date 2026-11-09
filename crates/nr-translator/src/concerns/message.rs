//! Message content handling. Port of open-sse/translator/concerns/message.js.

use nr_core::Json;

/// Collapse an OpenAI content-part array: a lone text part becomes a plain string,
/// otherwise the array is returned as-is. Cache-aware targets retain marked blocks.
pub fn collapse_text_parts(parts: &[Json], opts: &CollapsureOptions) -> Json {
    if parts.len() == 1 {
        if let Some(part) = parts.first() {
            if let Some(part_obj) = part.as_object() {
                if part_obj.get("type").and_then(|v| v.as_str()) == Some("text") {
                    if !opts.preserve_cache_control || part_obj.get("cache_control").is_none() {
                        if let Some(text) = part_obj.get("text") {
                            return text.clone();
                        }
                    }
                }
            }
        }
    }
    Json::Array(parts.to_vec())
}

#[derive(Clone, Debug, Default)]
pub struct CollapsureOptions {
    pub preserve_cache_control: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_collapse_text_parts_single_text() {
        let parts = vec![json!({"type": "text", "text": "hello"})];
        let result = collapse_text_parts(&parts, &CollapsureOptions::default());
        assert_eq!(result, json!("hello"));
    }

    #[test]
    fn test_collapse_text_parts_multiple() {
        let parts = vec![
            json!({"type": "text", "text": "hello"}),
            json!({"type": "text", "text": "world"}),
        ];
        let result = collapse_text_parts(&parts, &CollapsureOptions::default());
        assert!(result.is_array());
        assert_eq!(result.as_array().unwrap().len(), 2);
    }
}
