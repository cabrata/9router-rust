//! Executor foundation shared by every `nr-exec-*` crate.
//!
//! - [`base`]: port of open-sse/executors/base.js — the [`base::BaseHooks`]
//!   trait (overridable pieces of JS `BaseExecutor`) and the retry/fallback
//!   loop [`base::base_execute`]. FROZEN: five other crates build on it.
//! - [`default_executor`]: port of open-sse/executors/default.js.
//! - Shared modules: [`cline_auth`], [`cline_envelope`], [`machine_id`].

pub mod base;
pub mod default_executor;
pub mod cline_auth;
pub mod cline_envelope;
pub mod machine_id;

pub use base::{base_execute, BaseHooks, RetryDelay};
pub use default_executor::DefaultExecutor;
pub use cline_auth::{build_cline_headers, get_cline_access_token};
pub use cline_envelope::unwrap_cline_envelope;
pub use machine_id::get_consistent_machine_id;
