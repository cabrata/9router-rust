//! Port of open-sse/executors/default.js — the executor for every
//! OpenAI-compatible provider without a specialized executor.
//!
//! Wave-0 skeleton: base behavior only. The executor-simple agent ports the
//! rest of default.js (auth descriptors, header hooks, json_schema fallback,
//! Nous attribution, OAuth refresh grants, …) into this file.

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use nr_core::{ExecRequest, ExecResult, Executor, Json, Result};

use crate::base::{base_execute, BaseHooks};

static EMPTY: LazyLock<Json> = LazyLock::new(|| Json::Object(Default::default()));

pub struct DefaultExecutor {
    provider: String,
    config: &'static Json,
}

impl DefaultExecutor {
    /// JS `new DefaultExecutor(provider)`: config is `PROVIDERS[provider] || PROVIDERS.openai`.
    pub fn new(provider: &str) -> Self {
        let config = nr_data::provider(provider).or_else(|| nr_data::provider("openai")).unwrap_or(&EMPTY);
        Self { provider: provider.to_string(), config }
    }
}

impl BaseHooks for DefaultExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn config(&self) -> &Json {
        self.config
    }
}

#[async_trait]
impl Executor for DefaultExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn no_auth(&self) -> bool {
        BaseHooks::no_auth(self)
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        base_execute(self, req).await
    }
}

/// Shared constructor used by `nr-executors::get_executor` and by stubs.
pub fn create(provider: &str) -> Arc<dyn Executor> {
    Arc::new(DefaultExecutor::new(provider))
}
