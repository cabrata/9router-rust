//! Specialized web cookie-based executors — port of open-sse/executors.
//!
//! Each executor module exposes `pub fn create(provider: &str) -> Arc<dyn Executor>`.
//!
//! Modules:
//! - `deepseek_web` / `chatgpt_web` / `perplexity_web` / `uc_web`: executor implementations
//! - `deepseek/*`: PoW solver and stream format utilities
//! - `web_tools`: Tool translation shims for web executors

pub mod chatgpt_web;
pub mod deepseek_web;
pub mod deepseek_utils;
pub mod perplexity_web;
pub mod uc_web;
pub mod web_tools;
