//! Port of open-sse/executors/iflow.js.
//!
//! IFlow API with HMAC-SHA256 signature in headers.

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use nr_core::{ExecRequest, ExecResult, Executor, Result, Credentials, Json};
use nr_exec_base::base::{base_execute, BaseHooks};
use sha2::Sha256;
use uuid::Uuid;

static EMPTY: LazyLock<Json> = LazyLock::new(|| Json::Object(Default::default()));

struct IFlowExecutor {
    config: &'static Json,
}

impl IFlowExecutor {
    fn new() -> Self {
        let config = nr_data::provider("iflow").or_else(|| nr_data::provider("openai")).unwrap_or(&EMPTY);
        Self { config }
    }

    /// Generate HMAC-SHA256 signature for iFlow API.
    /// Payload: `{user_agent}:{session_id}:{timestamp}`.
    fn create_iflow_signature(&self, user_agent: &str, session_id: &str, timestamp: u64, api_key: &str) -> String {
        if api_key.is_empty() {
            return String::new();
        }
        let payload = format!("{}:{}:{}", user_agent, session_id, timestamp);
        let mut mac = Hmac::<Sha256>::new_from_slice(api_key.as_bytes())
            .unwrap_or_else(|_| Hmac::<Sha256>::new_from_slice(b"").unwrap());
        mac.update(payload.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }
}

impl BaseHooks for IFlowExecutor {
    fn provider(&self) -> &str {
        "iflow"
    }

    fn config(&self) -> &Json {
        self.config
    }

    fn build_headers(&self, creds: &Credentials, stream: bool, _url: &str, _model: &str, _body: &Json) -> Result<nr_core::http::HeaderList> {
        let mut headers = nr_core::http::HeaderList::new();
        headers.insert("Content-Type".into(), "application/json".into());

        // Add config headers
        if let Some(cfg_headers) = self.config.get("headers").and_then(|h| h.as_object()) {
            for (k, v) in cfg_headers {
                if let Some(v_str) = v.as_str() {
                    headers.insert(k.clone(), v_str.to_string());
                }
            }
        }

        // Generate session ID and timestamp
        let session_id = format!("session-{}", Uuid::new_v4());
        let timestamp = chrono::Utc::now().timestamp_millis() as u64;

        // Get user agent from config
        let user_agent = self.config.get("headers")
            .and_then(|h| h.get("User-Agent"))
            .and_then(|u| u.as_str())
            .unwrap_or("iFlow-Cli");

        // Get API key (prefer apiKey, fallback to accessToken)
        let api_key = creds.api_key().or_else(|| creds.access_token()).unwrap_or("");

        // Create signature
        let signature = self.create_iflow_signature(user_agent, &session_id, timestamp, api_key);

        // Add iFlow-specific headers
        headers.insert("session-id".into(), session_id);
        headers.insert("x-iflow-timestamp".into(), timestamp.to_string());
        headers.insert("x-iflow-signature".into(), signature);

        // Add authorization
        if !api_key.is_empty() {
            headers.insert("Authorization".into(), format!("Bearer {}", api_key));
        }

        if stream {
            headers.insert("Accept".into(), "text/event-stream".into());
        }

        Ok(headers)
    }

    fn transform_request(&self, _model: &str, body: &Json, stream: bool, _creds: &Credentials) -> Result<Json> {
        // Inject stream_options for streaming requests to get usage data
        if stream && body.get("messages").is_some() && body.get("stream_options").is_none() {
            let mut transformed = body.clone();
            if let Some(obj) = transformed.as_object_mut() {
                obj.insert("stream_options".into(), serde_json::json!({"include_usage": true}));
            }
            Ok(transformed)
        } else {
            Ok(body.clone())
        }
    }
}

#[async_trait]
impl Executor for IFlowExecutor {
    fn provider(&self) -> &str {
        "iflow"
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
    Arc::new(IFlowExecutor::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_iflow_signature_generation() {
        let executor = IFlowExecutor::new();
        let sig = executor.create_iflow_signature("iFlow-Cli", "session-123", 1000, "test-key");
        // Verify it's a valid hex string (even length, hex chars)
        assert!(!sig.is_empty());
        assert_eq!(sig.len(), 64); // SHA256 produces 32 bytes = 64 hex chars
        // Verify all chars are hex
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_iflow_signature_empty_key() {
        let executor = IFlowExecutor::new();
        let sig = executor.create_iflow_signature("iFlow-Cli", "session-123", 1000, "");
        assert_eq!(sig, "");
    }

    #[test]
    fn test_iflow_signature_deterministic() {
        let executor = IFlowExecutor::new();
        let sig1 = executor.create_iflow_signature("iFlow-Cli", "session-123", 1000, "key");
        let sig2 = executor.create_iflow_signature("iFlow-Cli", "session-123", 1000, "key");
        assert_eq!(sig1, sig2);
    }

    #[test]
    fn test_iflow_signature_differs_with_key() {
        let executor = IFlowExecutor::new();
        let sig1 = executor.create_iflow_signature("iFlow-Cli", "session-123", 1000, "key1");
        let sig2 = executor.create_iflow_signature("iFlow-Cli", "session-123", 1000, "key2");
        assert_ne!(sig1, sig2);
    }
}
