//! Specialized executors — port of open-sse/executors. Ownership: see AGENTS.md.
//!
//! Each executor module exposes `pub fn create(provider: &str) -> Arc<dyn Executor>`.

pub mod aws_credentials;
pub mod aws_event_stream;
pub mod aws_sigv4;
pub mod kiro;
pub mod kiro_session_replay;
pub mod kiro_models;
pub mod bedrock;
pub mod cursor;
pub mod cursor_checksum;
pub mod cursor_protobuf;
pub mod cursor_models;
