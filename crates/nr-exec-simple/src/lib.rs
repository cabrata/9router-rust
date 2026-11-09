//! Specialized executors — port of open-sse/executors. Ownership: see AGENTS.md.
//!
//! Each executor module exposes `pub fn create(provider: &str) -> Arc<dyn Executor>`.

pub mod azure;
pub mod vertex;
pub mod gemini_cli;
pub mod iflow;
pub mod kimchi;
pub mod kimchi_models;
pub mod ollama_local;
pub mod xiaomi_tokenplan;
pub mod xiaomi_mimo;
pub mod mimo_free;
pub mod minimax_code;
pub mod minimax_code_usage;
pub mod codebuddy_cn;
pub mod codebuddy_intl;
pub mod github;
pub mod codex;
pub mod commandcode;
pub mod opencode;
pub mod opencode_go;
pub mod opencode_zen;
