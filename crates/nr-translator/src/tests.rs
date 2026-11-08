#[cfg(test)]
mod tests {
    use crate::{translate_request, translate_response, init_state, needs_translation, init_registry};
    use crate::registry::formats;
    use nr_core::Json;

    #[test]
    fn test_needs_translation() {
        assert!(needs_translation(formats::OPENAI, formats::CLAUDE));
        assert!(!needs_translation(formats::OPENAI, formats::OPENAI));
        assert!(needs_translation(formats::CLAUDE, formats::KIRO));
    }

    #[test]
    fn test_init_state() {
        let state = init_state(formats::OPENAI);
        assert_eq!(state.message_id, None);
        assert_eq!(state.model, None);
        assert!(!state.text_block_started);
    }

    #[test]
    fn test_init_state_responses_format() {
        let state = init_state(formats::OPENAI_RESPONSES);
        // Base state should still be initialized
        assert_eq!(state.message_id, None);
    }

    #[test]
    fn test_translate_request_same_format() {
        init_registry();
        let body = serde_json::json!({
            "model": "gpt-4",
            "messages": [
                {"role": "user", "content": "hello"}
            ]
        });

        let result = translate_request(
            formats::OPENAI,
            formats::OPENAI,
            "gpt-4",
            body.clone(),
            true,
            None,
            &Default::default()
        );

        // Same format should return body as-is (or normalized)
        assert!(result.get("model").is_some());
    }

    #[test]
    fn test_translate_response_same_format() {
        init_registry();
        let chunk = serde_json::json!({
            "id": "chatcmpl-123",
            "object": "chat.completion.chunk",
            "created": 1234567890,
            "model": "gpt-4",
            "choices": [
                {
                    "index": 0,
                    "delta": {"content": "hello"},
                    "finish_reason": null
                }
            ]
        });

        let mut state = init_state(formats::OPENAI);
        let results = translate_response(
            formats::OPENAI,
            formats::OPENAI,
            Some(&chunk),
            &mut state
        );

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].get("id"), Some(&Json::String("chatcmpl-123".to_string())));
    }

    #[test]
    fn test_cursor_response_passthrough() {
        init_registry();
        let chunk = serde_json::json!({
            "object": "chat.completion.chunk",
            "choices": [{"delta": {"content": "test"}}]
        });

        let mut state = init_state("cursor");
        let results = translate_response("cursor", "openai", Some(&chunk), &mut state);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].get("object"), Some(&Json::String("chat.completion.chunk".to_string())));
    }

    #[test]
    fn test_translate_response_none_chunk() {
        init_registry();
        let mut state = init_state(formats::OPENAI);
        let results = translate_response(
            formats::OPENAI,
            formats::OPENAI,
            None,
            &mut state
        );

        assert_eq!(results.len(), 0);
    }
}
