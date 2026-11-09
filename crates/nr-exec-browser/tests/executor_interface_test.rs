//! Integration tests for browser executors
//! Tests focus on protocol implementation and message formatting rather than browser automation.

#[cfg(test)]
mod tests {
    use nr_exec_browser::gemini_web;
    use serde_json::json;

    #[test]
    fn test_gemini_prompt_single_turn() {
        let messages = vec![
            json!({"role": "user", "content": "What is 2+2?"})
        ];
        let prompt = gemini_web::build_gemini_prompt(&messages);
        assert_eq!(prompt, "What is 2+2?");
    }

    #[test]
    fn test_gemini_prompt_with_system() {
        let messages = vec![
            json!({"role": "system", "content": "You are a math teacher"}),
            json!({"role": "user", "content": "What is 2+2?"})
        ];
        let prompt = gemini_web::build_gemini_prompt(&messages);
        assert!(prompt.contains("System:"));
        assert!(prompt.contains("You are a math teacher"));
        assert!(prompt.contains("What is 2+2?"));
    }

    #[test]
    fn test_gemini_prompt_multiturn() {
        let messages = vec![
            json!({"role": "user", "content": "Hello"}),
            json!({"role": "assistant", "content": "Hi there"}),
            json!({"role": "user", "content": "How are you?"})
        ];
        let prompt = gemini_web::build_gemini_prompt(&messages);
        assert!(prompt.contains("Previous conversation:"));
        assert!(prompt.contains("User: Hello"));
        assert!(prompt.contains("Assistant: Hi there"));
    }
}
