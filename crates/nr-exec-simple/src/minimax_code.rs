//! Port of open-sse/executors/minimax-code.js.
//!
//! MiniMax Code reports credit/quota exhaustion as 402/403 (and 429) with a body
//! that names the balance. The executor normalizes 402/403 quota exhaustion to 429
//! for the account-switching logic.

use std::sync::Arc;

use async_trait::async_trait;
use nr_core::{ExecRequest, ExecResult, Executor, Json, Result};

/// Check if credentials are from a lease connection (authType === "mcode-lease").
fn is_lease_connection(credentials: &nr_core::Credentials) -> bool {
    credentials.get("authType")
        .and_then(Json::as_str)
        .map(|t| t == "mcode-lease")
        .unwrap_or(false)
}

/// Check if text contains quota-related keywords in CN or EN.
fn has_quota_keywords(text: &str) -> bool {
    let text_lower = text.to_lowercase();
    text_lower.contains("insufficient")
        || text_lower.contains("balance")
        || text_lower.contains("credit")
        || text_lower.contains("quota")
        || text_lower.contains("exhaust")
        || text_lower.contains("limit")
        || text.contains("余额")
        || text.contains("积分")
        || text.contains("额度")
        || text.contains("不足")
        || text.contains("用完")
        || text.contains("上限")
}

/// Normalize quota responses: 402/403 with balance message → 429.
/// This allows chatCore's account-switching logic to work correctly.
async fn normalize_quota_response(response: nr_core::http::UpstreamResponse) -> nr_core::http::UpstreamResponse {
    let status = response.status;
    // Only rewrite 402/403/429; pass 429 through as-is; 402/403 without quota marker also pass through
    if status != 402 && status != 403 && status != 429 {
        return response;
    }

    // 429 always passes through unchanged
    if status == 429 {
        return response;
    }

    // Read the response body to check for quota keywords
    let body = response.bytes().await.unwrap_or_default();
    let body_str = String::from_utf8_lossy(&body);

    // Check if the body contains quota-related keywords
    if !has_quota_keywords(&body_str) {
        // No quota keywords found; this is a real auth failure, pass through
        return nr_core::http::UpstreamResponse::from_bytes(status, response.headers.clone(), body);
    }

    // Extract the error message from the body
    let mut msg = body_str.trim().to_string();
    if let Ok(v) = serde_json::from_slice::<Json>(&body) {
        if let Some(msg_from_json) = v.get("error")
            .and_then(|e| e.get("message"))
            .and_then(Json::as_str) {
            msg = msg_from_json.to_string();
        } else if let Some(msg_from_json) = v.get("message").and_then(Json::as_str) {
            msg = msg_from_json.to_string();
        } else if let Some(msg_from_json) = v.get("msg").and_then(Json::as_str) {
            msg = msg_from_json.to_string();
        } else if let Some(msg_from_json) = v.get("base_resp")
            .and_then(|br| br.get("status_msg"))
            .and_then(Json::as_str) {
            msg = msg_from_json.to_string();
        }
    }

    // Rewrite as 429 with a rate_limit_error
    let mut headers = response.headers.clone();
    headers.shift_remove("content-length");
    headers.shift_remove("content-encoding");
    headers.insert("content-type".into(), "application/json".into());

    let error_body = serde_json::json!({
        "type": "error",
        "error": {
            "type": "rate_limit_error",
            "message": format!("usage limit reached: {}", msg)
        }
    });

    let error_bytes = serde_json::to_vec(&error_body).unwrap_or_default();
    nr_core::http::UpstreamResponse::from_bytes(429, headers, error_bytes)
}

/// MiniMax Code executor: extends DefaultExecutor with quota normalization and lease support.
/// TODO: Implement lease token support when minimaxCodeLease is ported.
pub struct MinimaxCodeExecutor {
    inner: Arc<dyn Executor>,
}

impl MinimaxCodeExecutor {
    fn new(provider: &str) -> Self {
        Self {
            inner: nr_exec_base::default_executor::create(provider),
        }
    }
}

#[async_trait]
impl Executor for MinimaxCodeExecutor {
    fn provider(&self) -> &str {
        "minimax-code"
    }

    fn no_auth(&self) -> bool {
        self.inner.no_auth()
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        // TODO: Implement lease token logic when minimaxCodeLease is available
        // if is_lease_connection(req.credentials) {
        //     let lease = get_lease_token(req.credentials, req.log).await?;
        //     req.credentials = { ...req.credentials, accessToken: lease.accessToken };
        // }

        let result = self.inner.execute(req).await?;

        // Normalize quota responses
        let normalized_response = normalize_quota_response(result.response).await;
        Ok(ExecResult {
            response: normalized_response,
            ..result
        })
    }
}

/// Executor for the canonical provider id (aliases are resolved in nr-executors).
pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(MinimaxCodeExecutor::new("minimax-code"))
}
