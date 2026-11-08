//! Specialized executors — port of open-sse/executors. Ownership: see AGENTS.md.
//!
//! Each executor module exposes `pub fn create(provider: &str) -> Arc<dyn Executor>`.

pub mod gemini_web;
pub mod grok_web;
pub mod muse_web;
pub mod veoaifree_web;
