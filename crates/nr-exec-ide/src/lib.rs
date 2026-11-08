//! Specialized executors — port of open-sse/executors. Ownership: see AGENTS.md.
//!
//! Each executor module exposes `pub fn create(provider: &str) -> Arc<dyn Executor>`.

pub mod qoder;
pub mod zed;
pub mod trae;
pub mod windsurf;
pub mod antigravity;
