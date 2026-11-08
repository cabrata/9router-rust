//! Port of open-sse/executors/bedrock.js.
//! Wave-0 stub: delegates to DefaultExecutor until ported.

use std::sync::Arc;

use nr_core::Executor;

/// Executor for the canonical provider id (aliases are resolved in nr-executors).
pub fn create(provider: &str) -> Arc<dyn Executor> {
    nr_exec_base::default_executor::create(provider)
}
