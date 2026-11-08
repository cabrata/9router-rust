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
pub mod thought_signature_store;

// Re-export key functions at crate level
pub use client_detector::{detect_client_tool, is_native_passthrough};
pub use upstream_headers::upstream_response_headers;
pub use session_manager::{generate_binary_style_id, derive_session_id, resolve_session_id, resolve_continuation_id};
pub use provider::{detect_format, get_target_format};
pub use model::{parse_model, resolve_provider_alias};
pub use claude_cloaking::{cloak_claude_tools, decloak_tool_names};
pub use reasoning_content_injector::inject_reasoning_content;
