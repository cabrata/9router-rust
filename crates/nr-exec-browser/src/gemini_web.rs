//! Port of open-sse/executors/gemini-web.js.
//! Not ported yet: delegates to DefaultExecutor (wave-1 placeholder never compiled).

use std::sync::Arc;

use nr_core::Executor;

/// Executor for the canonical provider id (aliases are resolved in nr-executors).
pub fn create(provider: &str) -> Arc<dyn Executor> {
    nr_exec_base::default_executor::create(provider)
}
