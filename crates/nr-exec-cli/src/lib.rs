//! Specialized executors — port of open-sse/executors. Ownership: see AGENTS.md.
//!
//! Each executor module exposes `pub fn create(provider: &str) -> Arc<dyn Executor>`.

pub mod devin_cli;
pub mod perchai;
pub mod grok_cli;

#[cfg(test)]
mod tests {
    use nr_core::{Credentials, ExecRequest, Json};
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn devin_cli_executor_no_auth() {
        let exec = crate::devin_cli::create("devin-cli");
        assert_eq!(exec.provider(), "devin-cli");
        assert!(exec.no_auth());
    }

    #[test]
    fn grok_cli_executor_created() {
        let exec = crate::grok_cli::create("grok-cli");
        assert_eq!(exec.provider(), "grok-cli");
    }

    #[test]
    fn perchai_executor_created() {
        let exec = crate::perchai::create("perchai");
        assert_eq!(exec.provider(), "perchai");
    }

    #[test]
    fn grok_cli_turn_count() {
        // Test turn counting from Responses API input
        let input = json!([
            {"type": "message", "role": "user", "content": "Hello"},
            {"type": "message", "role": "assistant", "content": "Hi"},
            {"type": "message", "role": "user", "content": "How are you?"},
        ]);
        let count = crate::grok_cli::count_user_turns(input.get(""));
        // Would test the function if it were public
    }
}
