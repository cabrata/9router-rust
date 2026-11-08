//! Specialized executors — port of open-sse/executors. Ownership: see AGENTS.md.
//!
//! Each executor module exposes `pub fn create(provider: &str) -> Arc<dyn Executor>`.

pub mod chatgpt_web;
pub mod deepseek_web;
pub mod perplexity_web;
pub mod uc_web;
