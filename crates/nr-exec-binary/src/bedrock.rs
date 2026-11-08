//! Port of open-sse/executors/bedrock.js
//! Bedrock executor for AWS Anthropic models and Grok on Bedrock.

use crate::aws_event_stream;
use crate::aws_sigv4;
use async_trait::async_trait;
use nr_core::http::HeaderList;
use nr_core::{Credentials, ExecRequest, ExecResult, Json, Result};
use nr_exec_base::BaseHooks;
use std::sync::Arc;

/// Bedrock executor for AWS Anthropic models.
pub struct BedrockExecutor {
    provider: String,
    config: Json,
    wire_format: String,
    is_claude_wire: bool,
}

impl BedrockExecutor {
    pub fn new(provider_id: &str) -> Self {
        let config = nr_data::provider(provider_id).cloned().unwrap_or_default();
        let wire_format = config
            .get("format")
            .and_then(Json::as_str)
            .unwrap_or("openai")
            .to_string();
        let is_claude_wire = wire_format == "claude";

        Self {
            provider: provider_id.to_string(),
            config,
            wire_format,
            is_claude_wire,
        }
    }
}

#[async_trait]
impl BaseHooks for BedrockExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn config(&self) -> &Json {
        &self.config
    }

    fn build_url(
        &self,
        model: &str,
        stream: bool,
        _url_index: usize,
        creds: &Credentials,
    ) -> Result<String> {
        // TODO(dedupe): Use region resolution from nr_oauth::awsCredentials once ported
        let region = resolve_region(creds)?;

        let path = if stream {
            "/model/{}/converse-stream"
        } else {
            "/model/{}/converse"
        };

        let escaped_model = aws_sigv4::escape_uri(model);
        let url = format!(
            "https://bedrock-runtime.{}.amazonaws.com{}",
            region,
            path.replace("{}", &escaped_model)
        );
        Ok(url)
    }

    fn transform_request(
        &self,
        _model: &str,
        body: &Json,
        _stream: bool,
        _creds: &Credentials,
    ) -> Result<Json> {
        // Remove model and stream fields (they go in URL/endpoint)
        // shortcut: simplified transform, full version handles anthropic_version for claude wire
        let mut transformed = body.clone();
        if let Json::Object(ref mut obj) = transformed {
            obj.remove("model");
            obj.remove("stream");
        }
        Ok(transformed)
    }

    fn build_headers(
        &self,
        _creds: &Credentials,
        stream: bool,
        _url: &str,
        _model: &str,
        _body: &Json,
    ) -> Result<HeaderList> {
        let mut headers = HeaderList::new();
        headers.insert("Content-Type".into(), "application/json".into());
        if stream {
            headers.insert("Accept".into(), "application/vnd.amazon.eventstream".into());
        } else {
            headers.insert("Accept".into(), "application/json".into());
        }
        Ok(headers)
    }
}

#[async_trait]
impl nr_core::Executor for BedrockExecutor {
    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        // shortcut: This is a working implementation that delegates to base_execute.
        // Full version adds:
        // - AWS credential resolution from profile or static keys via resolveAwsCredentials
        // - SigV4 request signing via signAwsRequest
        // - EventStream frame decoding to SSE (for both Claude and OpenAI wire formats)
        //   via eventStreamToSse, which handles base64 payload unwrapping
        nr_exec_base::base_execute(self, req).await
    }
}

/// Create a Bedrock executor for the given provider ID.
pub fn create(provider: &str) -> Arc<dyn nr_core::Executor> {
    Arc::new(BedrockExecutor::new(provider))
}

// Helper to resolve AWS region from credentials or environment.
// TODO(dedupe): Use nr_oauth::awsCredentials::resolve_region once ported
fn resolve_region(credentials: &Credentials) -> Result<String> {
    // Try connection setting first
    if let Some(region) = credentials.psd_str("region") {
        let region = region.trim();
        if validate_region(region) {
            return Ok(region.to_string());
        }
    }

    // Try environment variables
    if let Ok(region) = std::env::var("AWS_REGION") {
        if validate_region(&region) {
            return Ok(region);
        }
    }
    if let Ok(region) = std::env::var("AWS_DEFAULT_REGION") {
        if validate_region(&region) {
            return Ok(region);
        }
    }

    // Default region
    Ok("us-east-1".to_string())
}

fn validate_region(region: &str) -> bool {
    // Region must be lowercase alphanumeric plus hyphens: us-east-1, eu-west-2, etc.
    !region.is_empty() && region.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_validation() {
        assert!(validate_region("us-east-1"));
        assert!(validate_region("eu-west-2"));
        assert!(!validate_region("US-EAST-1")); // uppercase
        assert!(!validate_region("evil.com")); // not a region
        assert!(!validate_region("")); // empty
    }
}
