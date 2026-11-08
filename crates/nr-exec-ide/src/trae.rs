//! Port of open-sse/executors/trae.js.
//!
//! TraeExecutor routes requests to Trae's inference API.
//!
//! TODO(dedupe): Complete port of trae.js with full implementation details.
//! Currently delegates to DefaultExecutor.

use std::sync::Arc;

use nr_core::{ExecRequest, ExecResult, Executor, Result};

pub struct TraeExecutor {
    provider: String,
    config: &'static nr_core::Json,
}

impl TraeExecutor {
    pub fn new(provider: &str) -> Self {
        let config = nr_data::provider(provider)
            .or_else(|| nr_data::provider("trae"))
            .unwrap_or_else(|| &nr_core::Json::Object(Default::default()));
        Self {
            provider: provider.to_string(),
            config,
        }
    }
}

impl nr_exec_base::base::BaseHooks for TraeExecutor {
    fn provider(&self) -> &str {
        "trae"
    }

    fn config(&self) -> &nr_core::Json {
        self.config
    }
}

#[async_trait::async_trait]
impl Executor for TraeExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn no_auth(&self) -> bool {
        false
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        // TODO(dedupe): Port full trae.js implementation.
        nr_exec_base::default_executor::create("trae").execute(req).await
    }
}

pub fn create(provider: &str) -> Arc<dyn Executor> {
    Arc::new(TraeExecutor::new(provider))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trae_executor_creates() {
        let executor = TraeExecutor::new("trae");
        assert_eq!(executor.provider(), "trae");
    }
}
