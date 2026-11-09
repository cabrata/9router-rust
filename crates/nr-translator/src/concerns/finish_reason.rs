//! Finish reason mapping between formats. Port of open-sse/translator/concerns/finishReason.js.

use crate::schema::{openai_finish, claude_stops};

/// Map Claude stop_reason to OpenAI finish_reason.
pub fn claude_to_openai_finish_reason(stop_reason: &str) -> &'static str {
    match stop_reason {
        claude_stops::END_TURN => openai_finish::STOP,
        claude_stops::MAX_TOKENS => openai_finish::LENGTH,
        claude_stops::TOOL_USE => openai_finish::TOOL_CALLS,
        claude_stops::STOP_SEQUENCE => openai_finish::STOP,
        claude_stops::REFUSAL => openai_finish::CONTENT_FILTER,
        _ => openai_finish::STOP,
    }
}

/// Map OpenAI finish_reason to Claude stop_reason.
pub fn openai_to_claude_finish_reason(finish_reason: &str) -> &'static str {
    match finish_reason {
        openai_finish::STOP => claude_stops::END_TURN,
        openai_finish::LENGTH => claude_stops::MAX_TOKENS,
        openai_finish::TOOL_CALLS => claude_stops::TOOL_USE,
        openai_finish::CONTENT_FILTER => claude_stops::REFUSAL,
        _ => claude_stops::END_TURN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_claude_to_openai_finish_reason() {
        assert_eq!(claude_to_openai_finish_reason(claude_stops::END_TURN), openai_finish::STOP);
        assert_eq!(claude_to_openai_finish_reason(claude_stops::MAX_TOKENS), openai_finish::LENGTH);
        assert_eq!(claude_to_openai_finish_reason(claude_stops::TOOL_USE), openai_finish::TOOL_CALLS);
    }

    #[test]
    fn test_openai_to_claude_finish_reason() {
        assert_eq!(openai_to_claude_finish_reason(openai_finish::STOP), claude_stops::END_TURN);
        assert_eq!(openai_to_claude_finish_reason(openai_finish::LENGTH), claude_stops::MAX_TOKENS);
        assert_eq!(openai_to_claude_finish_reason(openai_finish::TOOL_CALLS), claude_stops::TOOL_USE);
    }
}
