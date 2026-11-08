//! Role/block/event constants. Port of open-sse/translator/schema/ (values also in nr-data).

/// Role constants for OpenAI/Claude formats.
pub mod roles {
    pub const USER: &str = "user";
    pub const ASSISTANT: &str = "assistant";
    pub const TOOL: &str = "tool";
    pub const SYSTEM: &str = "system";
    pub const DEVELOPER: &str = "developer";
}

/// Role constants for Gemini/Antigravity formats.
pub mod gemini_roles {
    pub const USER: &str = "user";
    pub const MODEL: &str = "model";
}

/// OpenAI content block types.
pub mod openai_blocks {
    pub const TEXT: &str = "text";
    pub const IMAGE_URL: &str = "image_url";
    pub const IMAGE: &str = "image";
    pub const INPUT_AUDIO: &str = "input_audio";
    pub const AUDIO_URL: &str = "audio_url";
    pub const FILE: &str = "file";
    pub const FUNCTION: &str = "function";
}

/// Claude content block types.
pub mod claude_blocks {
    pub const TEXT: &str = "text";
    pub const IMAGE: &str = "image";
    pub const DOCUMENT: &str = "document";
    pub const TOOL_USE: &str = "tool_use";
    pub const TOOL_RESULT: &str = "tool_result";
    pub const TOOL_REFERENCE: &str = "tool_reference";
    pub const CONTAINER_UPLOAD: &str = "container_upload";
    pub const THINKING: &str = "thinking";
    pub const REDACTED_THINKING: &str = "redacted_thinking";
    pub const SERVER_TOOL_USE: &str = "server_tool_use";
    pub const WEB_SEARCH_TOOL_RESULT: &str = "web_search_tool_result";
}

/// CommandCode content block types.
pub mod commandcode_blocks {
    pub const TOOL_CALL: &str = "tool-call";
    pub const TOOL_RESULT: &str = "tool-result";
}

/// OpenAI Responses API item types.
pub mod responses_items {
    pub const MESSAGE: &str = "message";
    pub const FUNCTION_CALL: &str = "function_call";
    pub const FUNCTION_CALL_OUTPUT: &str = "function_call_output";
    pub const CUSTOM_TOOL_CALL: &str = "custom_tool_call";
    pub const CUSTOM_TOOL_CALL_OUTPUT: &str = "custom_tool_call_output";
    pub const ADDITIONAL_TOOLS: &str = "additional_tools";
    pub const REASONING: &str = "reasoning";
    pub const OUTPUT_TEXT: &str = "output_text";
    pub const INPUT_TEXT: &str = "input_text";
    pub const INPUT_IMAGE: &str = "input_image";
    pub const SUMMARY_TEXT: &str = "summary_text";
}

/// OpenAI finish_reason values.
pub mod openai_finish {
    pub const STOP: &str = "stop";
    pub const LENGTH: &str = "length";
    pub const TOOL_CALLS: &str = "tool_calls";
    pub const CONTENT_FILTER: &str = "content_filter";
}

/// Claude stop_reason values.
pub mod claude_stops {
    pub const END_TURN: &str = "end_turn";
    pub const MAX_TOKENS: &str = "max_tokens";
    pub const TOOL_USE: &str = "tool_use";
    pub const STOP_SEQUENCE: &str = "stop_sequence";
    pub const REFUSAL: &str = "refusal";
}

/// Gemini finishReason values.
pub mod gemini_finish {
    pub const STOP: &str = "STOP";
    pub const MAX_TOKENS: &str = "MAX_TOKENS";
    pub const SAFETY: &str = "SAFETY";
    pub const RECITATION: &str = "RECITATION";
    pub const BLOCKLIST: &str = "BLOCKLIST";
    pub const PROHIBITED_CONTENT: &str = "PROHIBITED_CONTENT";
    pub const MALFORMED_FUNCTION_CALL: &str = "MALFORMED_FUNCTION_CALL";
    pub const UNEXPECTED_TOOL_CALL: &str = "UNEXPECTED_TOOL_CALL";
    pub const OTHER: &str = "OTHER";
}

/// OpenAI Responses API statuses.
pub mod responses_statuses {
    pub const COMPLETED: &str = "completed";
    pub const INCOMPLETE: &str = "incomplete";
    pub const FAILED: &str = "failed";
}

pub mod responses_incomplete_reasons {
    pub const MAX_OUTPUT_TOKENS: &str = "max_output_tokens";
    pub const CONTENT_FILTER: &str = "content_filter";
}

/// Anthropic Messages SSE events.
pub mod claude_events {
    pub const CONTENT_BLOCK_START: &str = "content_block_start";
    pub const CONTENT_BLOCK_DELTA: &str = "content_block_delta";
    pub const MESSAGE_DELTA: &str = "message_delta";
}

/// Anthropic content_block_delta delta types.
pub mod claude_deltas {
    pub const TEXT: &str = "text_delta";
    pub const INPUT_JSON: &str = "input_json_delta";
}

/// OpenAI Responses API events.
pub mod responses_events {
    pub const PREFIX: &str = "response.";
    pub const OUTPUT_ITEM_ADDED: &str = "response.output_item.added";
    pub const OUTPUT_ITEM_DONE: &str = "response.output_item.done";
    pub const OUTPUT_TEXT_DELTA: &str = "response.output_text.delta";
    pub const OUTPUT_TEXT_DONE: &str = "response.output_text.done";
    pub const FUNCTION_CALL_ARGUMENTS_DELTA: &str = "response.function_call_arguments.delta";
    pub const CUSTOM_TOOL_CALL_INPUT_DELTA: &str = "response.custom_tool_call_input.delta";
    pub const COMPLETED: &str = "response.completed";
    pub const INCOMPLETE: &str = "response.incomplete";
    pub const FAILED: &str = "response.failed";
}

/// Shared defaults.
pub mod defaults {
    pub const MODEL_FALLBACK: &str = "unknown";
    pub const DEFAULT_IMAGE_MIME: &str = "image/png";
}
