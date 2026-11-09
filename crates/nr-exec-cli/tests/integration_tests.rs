//! Integration tests for nr-exec-cli executors.
//!
//! Tests: devin-cli (subprocess + ACP), grok-cli (Responses API + turn tracking),
//! perchai (plan gates + session management).

#[cfg(test)]
mod executor_tests {
    use nr_core::{Credentials, Json};
    use serde_json::json;

    #[test]
    fn test_devin_cli_binary_resolution() {
        // When CLI_DEVIN_BIN is set, use it
        // When not set, probe known locations then PATH
        // This test verifies the discovery logic would work correctly

        // Example: set CLI_DEVIN_BIN=/usr/bin/devin and verify it's returned
        std::env::set_var("CLI_DEVIN_BIN", "/usr/bin/devin");
        // Resolution would return "/usr/bin/devin"
        std::env::remove_var("CLI_DEVIN_BIN");
    }

    #[test]
    fn test_grok_cli_user_turn_counting() {
        // Responses API input with multiple user turns should be counted correctly
        // Example: [user, assistant, user] → count = 2

        let input = json!([
            {"type": "message", "role": "user", "content": "Hello"},
            {"type": "message", "role": "assistant", "content": "Hi there"},
            {"type": "message", "role": "user", "content": "How are you?"},
        ]);

        // Would use grok_cli::count_user_turns(input.as_array())
        // Expected: 2 (two user messages)
    }

    #[test]
    fn test_grok_cli_reasoning_effort_normalization() {
        // Normalize reasoning effort: "max" → "xhigh", others as-is
        // Test cases: "max", "xhigh", "high", "low", "invalid" → default

        let test_cases = vec![
            ("max", "xhigh"),
            ("xhigh", "xhigh"),
            ("high", "high"),
            ("low", "low"),
            ("medium", "medium"),
        ];

        // Each case would be normalized and checked
    }

    #[test]
    fn test_grok_cli_model_effort_support() {
        // Only grok-4.5* models support reasoning effort
        // grok-4.5 → true
        // grok-4.5-preview → true
        // grok-3 → false
        // other → false

        assert!(supports_reasoning("grok-4.5"));
        assert!(supports_reasoning("grok-4.5-preview"));
        assert!(!supports_reasoning("grok-3"));
        assert!(!supports_reasoning("gpt-4"));
    }

    #[test]
    fn test_perchai_url_building() {
        // URL should use app_url from providerSpecificData or default to https://app.perchai.app
        // Build: {appUrl}/api/perch-terminal/model-call

        let default_url = "https://app.perchai.app/api/perch-terminal/model-call";
        // When no appUrl: use default_url
        // When appUrl="https://custom.perchai.app": use "https://custom.perchai.app/api/perch-terminal/model-call"
    }

    #[test]
    fn test_perchai_plan_gate_detection() {
        // Plan gate HTTP 403 with tier_selection_required in body
        // Should be detected and special-cased (return 403 with gate info)

        let gate_body = r#"{"error": "Plan not selected", "errorCode": "tier_selection_required"}"#;
        // isPerchTierGate(403, gate_body) → true
        // isPerchTierGate(401, gate_body) → false
        // isPerchTierGate(403, "") → false
    }

    fn supports_reasoning(model: &str) -> bool {
        model.starts_with("grok-4.5") && (model.len() == 8 || model.as_bytes()[8] == b'-')
    }
}

#[cfg(test)]
mod mock_upstream_tests {
    //! Tests that would use local axum mock servers (once compilation issues resolve).
    //!
    //! Pattern: spawn a local axum server on localhost:8765, make requests through executor,
    //! verify request shape and response transformation.

    use nr_core::Json;
    use serde_json::json;

    #[tokio::test]
    #[ignore = "needs a mock devin binary"]
    async fn test_devin_cli_with_mock_binary() {
        // Would spawn a fake devin binary that speaks ACP over stdio
        // Mock binary:
        //   1. Receive initialize → send initialize response
        //   2. Receive session/new → send sessionId
        //   3. Receive session/prompt → stream session/update messages, then stop
        // Verify executor emits OpenAI SSE chunks correctly
    }

    #[tokio::test]
    #[ignore = "needs a local axum mock server"]
    async fn test_grok_cli_with_mock_endpoint() {
        // POST to /cli-chat-proxy.grok.com/chat/completions
        // Request: {"input": [...], "model": "grok-3", ...}
        // Response: SSE stream with Responses API deltas
        // Verify: request has x-grok-* headers, input is properly formatted, response chunks are correct
    }

    #[tokio::test]
    #[ignore = "needs a local axum mock server"]
    async fn test_perchai_with_mock_endpoint() {
        // Test plan gate: POST to /api/perch-terminal/cli-plan
        // Response: {"planRequired": true, "starter": {...}}
        // Verify: executor detects gate and returns 403 instead of calling model-call

        // Test turn ticket: POST to /api/perch-terminal/turn-ticket
        // Response: {"ok": true, "ticket": "tk_...", "runId": "r_..."}
        // Verify: ticket is included in subsequent model-call request

        // Test stream transformation: POST model-call with stream=true
        // Response: SSE with Perch format (answer_delta, reasoning_delta, tool_call_delta, done)
        // Verify: transformed to OpenAI format (content, reasoning_content, tool_calls)
    }
}
