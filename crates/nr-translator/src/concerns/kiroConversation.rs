//! Kiro conversation/history management. Port of open-sse/translator/concerns/kiroConversation.js.
//! Reconciles conversation shape between client format and Kiro's GenerateAssistantResponse API.
//! TODO(dedupe): full KAS conversation shape normalization and edge case handling

use nr_core::Json;

/// Build Kiro request body from OpenAI/generic messages shape.
/// TODO: full implementation with conversation reconciliation and session replay
pub fn build_kiro_conversation(_messages: &[Json]) -> Json {
    serde_json::json!({})
}

/// Parse Kiro GenerateAssistantResponse response into OpenAI-compatible chunks.
/// TODO: full implementation with streaming chunk reconstruction
pub fn parse_kiro_response(_response: &Json) -> Vec<Json> {
    vec![]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_kiro_conversation_empty() {
        let messages: Vec<Json> = vec![];
        let conv = build_kiro_conversation(&messages);
        assert!(conv.is_object());
    }
}
