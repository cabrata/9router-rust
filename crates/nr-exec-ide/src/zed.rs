//! Port of open-sse/executors/zed.js (+ shared/zedAuth.js).
//! Not ported yet: delegates to DefaultExecutor. An earlier partial draft is in
//! commit 215bef0 (crates/nr-exec-ide/src/zed.rs, zed_auth.rs) for reference.

use std::sync::Arc;

use nr_core::Executor;

/// Executor for the canonical provider id (aliases are resolved in nr-executors).
pub fn create(provider: &str) -> Arc<dyn Executor> {
    nr_exec_base::default_executor::create(provider)
}
