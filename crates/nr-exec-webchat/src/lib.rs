//! Specialized web cookie-based executors — port of open-sse/executors.
//!
//! This crate provides executors for web-based AI providers that use cookies/tokens for
//! authentication and return SSE streams. All executors currently delegate to the default
//! executor (which handles OpenAI-compatible endpoints) as a placeholder; full implementations
//! are marked with `TODO(dedupe)` where dependencies are needed.
//!
//! Each executor module exposes `pub fn create(provider: &str) -> Arc<dyn Executor>`.
//!
//! ## Modules:
//! - `deepseek_web`: DeepSeek chat.deepseek.com executor (PoW-gated, Bearer token)
//!   - TODO: Implement PoW solver, SSE transform, session caching
//! - `chatgpt_web`: ChatGPT chatgpt.com executor (browser session auth)
//!   - TODO: Implement browser session handling, DALL-E image generation
//!   - shortcut: Browser-related functionality not ported; would depend on nr-exec-browser
//! - `perplexity_web`: Perplexity API web executor
//!   - TODO: Implement web search session, citation formatting
//! - `uc_web`: UC Chat web executor
//!   - TODO: Implement UC Chat session and streaming format
//! - `deepseek_utils`: DeepSeek-specific utilities (model detection, stream formatting, PoW)
//! - `web_tools`: Tool translation shims for web executors (deepseekWebTools.js, webTools.js)
//!   - TODO: Implement DSML parsing and serialization, tool call formatting

pub mod attachments;
pub mod chatgpt_web;
pub mod deepseek_web;
pub mod deepseek_utils;
pub mod delta_v1;
pub mod image_sniff;
pub mod sentinel;
pub mod perplexity_web;
pub mod uc_web;
pub mod web_tools;
