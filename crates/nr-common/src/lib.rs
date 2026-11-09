//! Leaf utilities shared by the translator, executors and engine: the
//! dependency-free parts of open-sse/utils plus open-sse/services/provider.js
//! and model.js. Owner: data agent (see AGENTS.md).

pub mod sse_constants;
pub mod sse;
pub mod session_manager;
pub mod provider;
pub mod model;
pub mod claude_cloaking;
pub mod reasoning_content_injector;
pub mod claude_signature;
pub mod web_cookie_auth;
pub mod gemini_cookies;
pub mod usage_tracking;
pub mod upstream_headers;
pub mod client_detector;
pub mod model_markers;
pub mod tag_blocks;
pub mod debug_log;
pub mod writable_data_dir;
pub mod systemone_router;
pub mod capacity_adapter;
pub mod thought_signature_store;

// Re-export key functions at crate level
pub use client_detector::{detect_client_tool, is_native_passthrough};
pub use upstream_headers::upstream_response_headers;
pub use session_manager::{generate_binary_style_id, derive_session_id, resolve_session_id, resolve_continuation_id};
pub use provider::{detect_format, get_target_format, normalize_thinking_config};
pub use model::{parse_model, resolve_provider_alias};
pub use claude_cloaking::{cloak_claude_tools, decloak_tool_names, decloak_stream_chunk, apply_cloaking};
pub use reasoning_content_injector::inject_reasoning_content;
pub use claude_signature::{has_claude_signature_prefix, is_valid_claude_signature};
pub use model_markers::strip_model_context_marker;
pub use tag_blocks::find_tag_blocks;
pub use web_cookie_auth::{
    normalize_session_cookie_header, normalize_session_cookie_headers, extract_cookie_value,
    build_grok_cookie_header, to_playwright_cookies, strip_cookie_input_prefix,
};
pub use gemini_cookies::{normalize_gemini_cookie_input, normalize_gemini_validation_cookie};
pub use usage_tracking::{
    canonicalize_usage, extract_usage, normalize_usage, merge_usage, add_buffer_to_usage,
    filter_usage_for_format, estimate_usage, estimate_input_tokens, estimate_output_tokens,
    format_usage, has_valid_usage, log_usage,
};
pub use writable_data_dir::get_writable_data_dir;
pub use debug_log::{dbg, is_dev_enabled};
