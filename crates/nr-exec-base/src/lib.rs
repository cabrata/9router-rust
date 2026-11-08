//! Executor foundation shared by every `nr-exec-*` crate.
//!
//! - [`base`]: port of open-sse/executors/base.js — the [`base::BaseHooks`]
//!   trait (overridable pieces of JS `BaseExecutor`) and the retry/fallback
//!   loop [`base::base_execute`]. FROZEN: five other crates build on it.
//! - [`default_executor`]: port of open-sse/executors/default.js.

pub mod base;
pub mod default_executor;

pub use base::{base_execute, BaseHooks, RetryDelay};
pub use default_executor::DefaultExecutor;
