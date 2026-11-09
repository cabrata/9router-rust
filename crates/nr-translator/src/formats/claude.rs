//! Claude format helpers. Port of open-sse/translator/formats/claude.js (749 lines).
//! TODO(dedupe): full implementation with cache control, tool ordering, and message normalization

use nr_core::Json;

/// Prepare request for Claude format endpoints
/// TODO: full implementation with cache_control management and message normalization
pub fn prepare_claude_request(_body: &mut Json, _provider: Option<&str>, _api_key: Option<&str>, _connection_id: Option<&str>, _raw_headers: Option<&Json>, _session_id: Option<&str>) {
    // Stub: implementation needed (cache_control management + message normalization)
}

/// Ensure trailing user turn for Claude (no empty assistant prefill)
/// TODO: full implementation with clientLastRole handling
pub fn ensure_trailing_user_turn(_messages: &mut Vec<Json>, _client_last_role: Option<&str>) {
    // Stub: implementation needed (clientLastRole handling)
}

/// Find last cacheable tool index
pub fn last_cacheable_tool_index(tools: Option<&[Json]>) -> Option<usize> {
    let tools = tools?;
    for (idx, tool) in tools.iter().enumerate().rev() {
        if !tool.get("defer_loading").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Some(idx);
        }
    }
    None
}

/// Check if message has valid non-empty content
pub fn has_valid_content(msg: &Json) -> bool {
    if let Some(content_str) = msg.get("content").and_then(|v| v.as_str()) {
        return !content_str.trim().is_empty();
    }

    if let Some(content_arr) = msg.get("content").and_then(|v| v.as_array()) {
        return content_arr.iter().any(|block| is_contentful_block(block));
    }

    false
}

fn is_contentful_block(block: &Json) -> bool {
    if let Some(block_obj) = block.as_object() {
        match block_obj.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                block_obj.get("text")
                    .and_then(|v| v.as_str())
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false)
            }
            Some("tool_use") | Some("tool_result") | Some("image") | Some("document") | Some("container_upload") => true,
            _ => false
        }
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_has_valid_content_text() {
        let msg = json!({"content": "hello"});
        assert!(has_valid_content(&msg));
    }

    #[test]
    fn test_has_valid_content_empty() {
        let msg = json!({"content": ""});
        assert!(!has_valid_content(&msg));
    }

    #[test]
    fn test_last_cacheable_tool_index() {
        let tools = vec![
            json!({"defer_loading": true}),
            json!({"defer_loading": false}),
            json!({"defer_loading": true}),
        ];
        let result = last_cacheable_tool_index(Some(&tools));
        assert_eq!(result, Some(1));
    }
}
