//! Port of open-sse/executors/qoder.js (+ shared/qoder/**).
//!
//! QoderExecutor sends OpenAI-format requests to Qoder's COSY-signed inference endpoint,
//! then unwraps Qoder's `{statusCodeValue, body}` SSE envelope back to OpenAI SSE.
//!
//! Key features:
//! - COSY authentication (RSA + AES + MD5 headers)
//! - Request normalization (hoisting system messages, normalizing content blocks)
//! - Per-model config fetching from /algo/api/v2/model/list
//! - Queue-status polling for long-running requests
//! - Context tier resolution
//!
//! Ownership: exec-ide. Dependencies: shared/qoder/**, services/qoderModels.js (ported to nr-data).

use std::sync::Arc;

use nr_core::{ExecRequest, ExecResult, Executor, Result};

/// TODO(dedupe): Complete port of qoder.js with:
/// - normalizeMessages() — hoist system messages, flatten content
/// - buildQoderRequestBody() — image upload rewrite, model config fetch, business block
/// - buildCosyHeaders() — RSA + AES crypto + signing
/// - createQoderSseCoalescer() — wrap SSE stream unwrapper
/// - Queue-status polling loop for long requests
pub struct QoderExecutor {
    provider: String,
    config: &'static nr_core::Json,
}

impl QoderExecutor {
    pub fn new(provider: &str) -> Self {
        let config = nr_data::provider(provider)
            .or_else(|| nr_data::provider("qoder"))
            .unwrap_or_else(|| &nr_core::Json::Object(Default::default()));
        Self {
            provider: provider.to_string(),
            config,
        }
    }
}

impl nr_exec_base::base::BaseHooks for QoderExecutor {
    fn provider(&self) -> &str {
        "qoder"
    }

    fn config(&self) -> &nr_core::Json {
        self.config
    }
}

#[async_trait::async_trait]
impl Executor for QoderExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn no_auth(&self) -> bool {
        false
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        // TODO(dedupe): Implement full qoder executor once:
        // - nr_data exports qoderModels.js functions
        // - COSY auth module available (shared/qoder/cosy.js)
        // - qoder encoding available (shared/qoder/encoding.js)
        // - Translator integration for request/response shape

        // For now, delegate to default executor
        nr_exec_base::default_executor::create("qoder").execute(req).await
    }
}

pub fn create(provider: &str) -> Arc<dyn Executor> {
    Arc::new(QoderExecutor::new(provider))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qoder_executor_creates() {
        let executor = QoderExecutor::new("qoder");
        assert_eq!(executor.provider(), "qoder");
    }

    #[test]
    fn qoder_executor_base_hooks() {
        use nr_exec_base::base::BaseHooks;
        let executor = QoderExecutor::new("qoder");
        assert_eq!(executor.provider(), "qoder");
        assert_eq!(executor.no_auth(), false);
        assert!(executor.base_urls().len() > 0);
    }

    #[test]
    fn qoder_create_function() {
        let executor = create("qoder");
        assert_eq!(executor.provider(), "qoder");
    }
}
