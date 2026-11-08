//! Port of open-sse/executors/antigravity.js.
//!
//! AntigravityExecutor routes requests through Google's Gemini (via Anthropic's
//! Antigravity integration). Key features:
//!
//! - IDE request ID generation and tracking (agent/org/project/dev/sessionId)
//! - Thought signature generation for extended thinking
//! - Request normalization (sanitize function names, strip unsupported fields)
//! - Quota routing across multiple models
//! - Retry logic with transient error detection
//! - Image model detection and configuration
//! - Session ID resolution
//!
//! TODO(dedupe): Complete implementation requires:
//! - services/thoughtSignatureStore.js (get thought signature)
//! - Gemini format translation (normalizeGeminiContents, cleanJSONSchemaForAntigravity)
//! - Session ID resolution (resolveSessionId)
//! - BaseHooks::compute_retry_delay override for transient error detection
//! - Request/response translation for Gemini format

use std::sync::Arc;

use nr_core::{ExecRequest, ExecResult, Executor, Result};

pub struct AntigravityExecutor {
    provider: String,
    config: &'static nr_core::Json,
}

impl AntigravityExecutor {
    pub fn new(provider: &str) -> Self {
        let config = nr_data::provider(provider)
            .or_else(|| nr_data::provider("antigravity"))
            .unwrap_or_else(|| &nr_core::Json::Object(Default::default()));
        Self {
            provider: provider.to_string(),
            config,
        }
    }

    /// Sanitize function name for Gemini: [a-zA-Z_][a-zA-Z0-9_.:\-]{0,63}
    fn sanitize_function_name(name: &str) -> String {
        if name.is_empty() {
            return "_unknown".to_string();
        }
        let mut s = name.replace(|c: char| !c.is_alphanumeric() && c != '_' && c != '.' && c != ':' && c != '-', "_");
        if !s.starts_with(|c: char| c.is_alphabetic() || c == '_') {
            s.insert(0, '_');
        }
        s.chars().take(64).collect()
    }

    /// Detect if a model is an image generation model.
    fn is_image_model(model: &str) -> bool {
        let m_lower = model.to_lowercase();
        m_lower.contains("image") || m_lower.contains("imagen")
    }

    /// Parse aspect ratio from model name (e.g. "...-16x9" → "16:9").
    fn parse_image_config(model: &str) -> String {
        if let Some(caps) = regex::Regex::new(r"(\d+)x(\d+)$")
            .ok()
            .and_then(|re| re.captures(model))
        {
            if let (Ok(w), Ok(h)) = (caps[1].parse::<f64>(), caps[2].parse::<f64>()) {
                if w <= 16.0 && h <= 16.0 {
                    return format!("{}:{}", w as i32, h as i32);
                }
                // Compute GCD for aspect ratio reduction
                fn gcd(a: i32, b: i32) -> i32 {
                    if b == 0 { a } else { gcd(b, a % b) }
                }
                let wi = w as i32;
                let hi = h as i32;
                let d = gcd(wi, hi);
                return format!("{}:{}", wi / d, hi / d);
            }
        }
        "1:1".to_string()
    }
}

impl nr_exec_base::base::BaseHooks for AntigravityExecutor {
    fn provider(&self) -> &str {
        "antigravity"
    }

    fn config(&self) -> &nr_core::Json {
        self.config
    }

    /// Override retry logic to detect transient Antigravity errors.
    async fn compute_retry_delay(
        &self,
        status: u16,
        _headers: &http::HeaderMap,
        body: &[u8],
        _attempt: u32,
        delay_ms: u64,
    ) -> nr_exec_base::base::RetryDelay {
        // TODO(dedupe): Implement transient error pattern matching.
        // Check both status code and error message for patterns like:
        // "high traffic", "agent execution terminated", "capacity", etc.

        // For now, use default retry logic
        if (500..=599).contains(&status) || status == 429 {
            nr_exec_base::base::RetryDelay::Ms(delay_ms.min(15000))
        } else {
            nr_exec_base::base::RetryDelay::Default
        }
    }
}

#[async_trait::async_trait]
impl Executor for AntigravityExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn no_auth(&self) -> bool {
        false
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        // TODO(dedupe): Implement full antigravity flow:
        // 1. Resolve session ID
        // 2. Generate IDE request ID
        // 3. Fetch thought signature if thinking enabled
        // 4. Normalize request (sanitize functions, strip blacklisted fields)
        // 5. Handle image models
        // 6. Build Gemini request
        // 7. Execute with quota routing and retry logic
        // 8. Translate response back to OpenAI format

        // For now, delegate to default executor
        nr_exec_base::default_executor::create("antigravity").execute(req).await
    }
}

pub fn create(provider: &str) -> Arc<dyn Executor> {
    Arc::new(AntigravityExecutor::new(provider))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_function_name_valid() {
        assert_eq!(AntigravityExecutor::sanitize_function_name("hello_world"), "hello_world");
        assert_eq!(AntigravityExecutor::sanitize_function_name("my.function"), "my.function");
    }

    #[test]
    fn sanitize_function_name_starts_with_digit() {
        let result = AntigravityExecutor::sanitize_function_name("123func");
        assert!(result.starts_with('_'));
    }

    #[test]
    fn sanitize_function_name_special_chars() {
        let result = AntigravityExecutor::sanitize_function_name("func!@#$%");
        assert!(!result.contains('!'));
    }

    #[test]
    fn is_image_model_detection() {
        assert!(AntigravityExecutor::is_image_model("gemini-3.1-image"));
        assert!(AntigravityExecutor::is_image_model("imagen-3"));
        assert!(!AntigravityExecutor::is_image_model("gemini-3.1-flash"));
    }

    #[test]
    fn parse_image_config_ratio() {
        assert_eq!(AntigravityExecutor::parse_image_config("model-16x9"), "16:9");
        assert_eq!(AntigravityExecutor::parse_image_config("model-1024x768"), "4:3");
    }

    #[test]
    fn antigravity_executor_creates() {
        let executor = AntigravityExecutor::new("antigravity");
        assert_eq!(executor.provider(), "antigravity");
    }
}
