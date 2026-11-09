//! Port of open-sse/executors/xiaomi-tokenplan.js.
//!
//! Token Plan keys are region-specific. Routes per sourceFormat-matched transport:
//! claude → Anthropic /anthropic/v1/messages, openai → /chat/completions.

use std::sync::Arc;
use std::sync::LazyLock;

use async_trait::async_trait;
use nr_core::{ExecRequest, ExecResult, Executor, Result, Credentials, Json};
use nr_exec_base::base::{base_execute, BaseHooks};

static EMPTY: LazyLock<Json> = LazyLock::new(|| Json::Object(Default::default()));

/// Resolve the Xiaomi Tokenplan base URL from credentials, falling back to provider config.
fn resolve_xiaomi_tokenplan_base_url(creds: &Credentials) -> String {
    if let Some(base_url) = creds.psd_str("baseUrl") {
        return base_url.to_string();
    }
    // Fallback to config or a safe default
    nr_data::provider("xiaomi-tokenplan")
        .and_then(|p| p.get("baseUrl"))
        .and_then(Json::as_str)
        .unwrap_or("https://api.xiaomixiaomi.com/v1")
        .to_string()
}

struct XiaomiTokenplanExecutor {
    config: &'static Json,
}

impl XiaomiTokenplanExecutor {
    fn new() -> Self {
        let config = nr_data::provider("xiaomi-tokenplan")
            .or_else(|| nr_data::provider("openai"))
            .unwrap_or(&EMPTY);
        Self { config }
    }
}

impl BaseHooks for XiaomiTokenplanExecutor {
    fn provider(&self) -> &str {
        "xiaomi-tokenplan"
    }

    fn config(&self) -> &Json {
        self.config
    }

    fn build_url(
        &self,
        _model: &str,
        _stream: bool,
        _url_index: usize,
        creds: &Credentials,
    ) -> Result<String> {
        let base_url = resolve_xiaomi_tokenplan_base_url(creds);

        // Route per sourceFormat-matched transport:
        // claude → Anthropic /anthropic/v1/messages, openai → /chat/completions
        if let Some(rt) = creds.get("runtimeTransport").and_then(Json::as_object) {
            if let Some(format) = rt.get("format").and_then(Json::as_str) {
                if format == "claude" {
                    let base = base_url.trim_end_matches('/');
                    return Ok(format!("{}/anthropic/v1/messages", base.trim_end_matches("/v1")));
                }
            }
        }

        let base = base_url.trim_end_matches('/');
        Ok(format!("{}/chat/completions", base))
    }
}

#[async_trait]
impl Executor for XiaomiTokenplanExecutor {
    fn provider(&self) -> &str {
        "xiaomi-tokenplan"
    }

    fn no_auth(&self) -> bool {
        BaseHooks::no_auth(self)
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        base_execute(self, req).await
    }
}

/// Executor for the canonical provider id (aliases are resolved in nr-executors).
pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(XiaomiTokenplanExecutor::new())
}
