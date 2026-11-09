//! Specialized web cookie-based executors — port of open-sse/executors.
//!
//! These executors talk to the providers' consumer web APIs using cookies or
//! bearer tokens and return SSE streams. Each module exposes
//! `pub fn create(provider: &str) -> Arc<dyn Executor>`.
//!
//! ## Modules
//! - `chatgpt_web`: chatgpt.com (cookie/access-token session, sentinel PoW, DALL-E)
//! - `deepseek_web`: chat.deepseek.com (PoW-gated, Bearer token, tool-repair loop)
//! - `perplexity_web`: perplexity.ai web search sessions and citations
//! - `uc_web`: UC Chat image/video generation via Clerk auth
//! - `chat_http`, `sentinel`, `attachments`, `image_sniff`, `delta_v1`: shared
//!   HTTP/session, PoW, upload and stream-delta plumbing
//! - `web_tools`, `deepseek_utils`, `chatgpt_web_tools`: prompt-contract tool
//!   translation shims (`webTools.js`, `deepseekWebTools.js`)
//!
//! shortcut: `chatgpt_web` drives the HTTP API rather than a real browser
//! session (headed Chromium lives in `nr-exec-browser`); upgrade when a
//! flow needs DOM automation that plain HTTP cannot express.

pub mod attachments;
pub mod chat_http;
pub mod chatgpt_web;
pub mod chatgpt_web_tools;
pub mod deepseek_web;
pub mod deepseek_utils;
pub mod delta_v1;
pub mod image_sniff;
pub mod sentinel;
pub mod perplexity_web;
pub mod uc_web;
pub mod web_tools;
