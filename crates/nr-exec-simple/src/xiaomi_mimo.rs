//! Port of open-sse/executors/xiaomi-mimo.js.
//!
//! Dual-route v2.6 models: account-service route (with session cookie) vs cloud API (with sk- key).
//! TODO(dedupe): Implement getMimoAccountCookie and mimoAccount helpers from shared/mimoAccount.js
//! once they are ported to nr-common.

use std::sync::Arc;

use async_trait::async_trait;
use nr_core::{ExecRequest, ExecResult, Executor, Json, Result, Credentials};

const COOKIE_KEY: &str = "__mimoAccountCookie";

/// V2.6 models that support dual-route account service.
const ACCOUNT_MODELS: &[&str] = &[
    "mimo-v2.6-pro",
    "mimo-v2.6-flash",
    "mimo-v2.6-pro-ultraspeed",
];

/// Strip provider prefix from model id (e.g., "xiaomi-mimo/mimo-v2.6-pro" → "mimo-v2.6-pro").
fn bare_model(model: &str) -> &str {
    if let Some(idx) = model.find('/') {
        &model[idx + 1..]
    } else {
        model
    }
}

/// Check if a model is an account-route model and has account credentials.
fn is_account_route(model: &str, credentials: &Credentials) -> bool {
    let bare = bare_model(model);
    if !ACCOUNT_MODELS.contains(&bare) {
        return false;
    }

    // Check if we have a session cookie or mimoPassToken
    credentials.get(COOKIE_KEY).is_some()
        || credentials.psd_str("mimoPassToken").is_some()
}

struct XiaomiMimoExecutor {
    inner: Arc<dyn Executor>,
}

impl XiaomiMimoExecutor {
    fn new(provider: &str) -> Self {
        Self {
            inner: nr_exec_base::default_executor::create(provider),
        }
    }
}

#[async_trait]
impl Executor for XiaomiMimoExecutor {
    fn provider(&self) -> &str {
        "xiaomi-mimo"
    }

    fn no_auth(&self) -> bool {
        self.inner.no_auth()
    }

    async fn execute(&self, mut req: ExecRequest<'_>) -> Result<ExecResult> {
        // TODO: Implement account routing when getMimoAccountCookie is available
        // if is_account_route(req.model, req.credentials) {
        //     // Get account cookie and set on credentials
        //     if let Ok(cookie) = get_mimo_account_cookie(credentials).await {
        //         req.credentials.insert(COOKIE_KEY.to_string(), cookie);
        //         let mut result = self.inner.execute(req).await?;
        //         // Retry on 401 with fresh cookie
        //         if result.response.status == 401 {
        //             // invalidate cache and retry
        //         }
        //     }
        // }

        self.inner.execute(req).await
    }
}

/// Executor for the canonical provider id (aliases are resolved in nr-executors).
pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(XiaomiMimoExecutor::new("xiaomi-mimo"))
}
