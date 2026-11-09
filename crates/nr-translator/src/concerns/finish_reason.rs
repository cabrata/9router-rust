//! Finish reason mapping between formats. Port of open-sse/translator/concerns/finishReason.js.

use crate::schema::{openai_finish, claude_stops, gemini_finish};

/// Map upstream finish/stop reason → OpenAI finish_reason.
pub fn to_openai_finish(reason: Option<&str>, format: &str) -> &'static str {
    let reason = reason.unwrap_or("");
    match format {
        "claude" => claude_to_openai_finish_reason(reason),
        "commandcode" => match reason {
            "stop" => openai_finish::STOP,
            "length" => openai_finish::LENGTH,
            "tool-calls" | "tool_use" => openai_finish::TOOL_CALLS,
            "content-filter" => openai_finish::CONTENT_FILTER,
            "error" => openai_finish::STOP,
            _ => openai_finish::STOP,
        },
        "gemini" => match reason.to_uppercase().as_str() {
            "STOP" => openai_finish::STOP,
            "MAX_TOKENS" => openai_finish::LENGTH,
            "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" => openai_finish::CONTENT_FILTER,
            _ => openai_finish::STOP,
        },
        "kiro" | "ollama" => match reason {
            "tool_calls" | "tool_use" => openai_finish::TOOL_CALLS,
            "length" | "max_tokens" => openai_finish::LENGTH,
            _ => openai_finish::STOP,
        },
        _ => openai_finish::STOP,
    }
}

/// Map OpenAI finish_reason → upstream stop reason.
/// JS `fromOpenAIFinish`: claude maps to stop_reason, every other format passes
/// the input through. Returned as `Option` so callers own the passthrough value.
pub fn from_openai_finish(reason: Option<&str>, format: &str) -> Option<String> {
    let reason = reason.unwrap_or("");
    if format == "claude" {
        return Some(openai_to_claude_finish_reason(reason).to_string());
    }
    if reason.is_empty() {
        Some(openai_finish::STOP.to_string())
    } else {
        Some(reason.to_string())
    }
}

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
