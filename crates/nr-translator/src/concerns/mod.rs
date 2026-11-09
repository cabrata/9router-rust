//! Shared translation logic. Port of open-sse/translator/concerns/.

pub mod chunk;
#[allow(non_snake_case)]
pub mod claudeMessage;
pub mod finish_reason;
pub mod image;
pub mod json;
#[allow(non_snake_case)]
pub mod kiroConversation;
pub mod kiro_session_replay;
pub mod message;
pub mod modality;
#[allow(non_snake_case)]
pub mod paramSupport;
pub mod prefetch;
pub mod reasoning;
pub mod thinking;
#[allow(non_snake_case)]
pub mod thinkingUnified;
pub mod tool_call;
pub mod usage;
