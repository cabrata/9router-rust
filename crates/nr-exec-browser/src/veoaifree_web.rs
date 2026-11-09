//! Port of open-sse/executors/veoaifree-web.js
//! veoaifree.com free media generator (WordPress AJAX API, no auth needed)
//!
//! Video: full-video-generate → final-video-results polling → mp4 artifact download
//! Image: nano-banana generator returning URLs or raw base64 parts
//! Rate limited (~6 requests/hour per IP)

use nr_core::Executor;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

const BASE_URL: &str = "https://veoaifree.com";
const AJAX_URL: &str = "https://veoaifree.com/wp-admin/admin-ajax.php";
const POLL_INTERVAL_MS: u64 = 20000;
const MAX_POLLS: u32 = 30; // 10 minutes total
const FETCH_TIMEOUT_MS: u64 = 30000;
const ARTIFACT_MAX_BYTES: usize = 100 * 1024 * 1024;
const ARTIFACT_MAX_REDIRECTS: u32 = 3;
const ARTIFACT_READY_MAX_ATTEMPTS: u32 = 12;
const ARTIFACT_READY_RETRY_INTERVAL_MS: u64 = 5000;

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36";

/// VeoAiFree executor for media generation
pub struct VeoAiFreeWebExecutor;

#[async_trait]
impl Executor for VeoAiFreeWebExecutor {
    async fn execute(
        &self,
        _req: &nr_core::ExecutorRequest,
    ) -> Result<nr_core::ExecutorResponse, Box<dyn std::error::Error>> {
        // veoaifree-web is a media provider, not a chat provider
        // It should only be accessed via /v1/images/generations or /v1/videos/generations
        let error_response = json!({
            "error": {
                "message": "veoaifree-web is a media provider — use /v1/images/generations or /v1/videos/generations",
                "type": "invalid_request"
            }
        });

        Ok(nr_core::ExecutorResponse {
            status: 400,
            headers: std::collections::HashMap::from([
                ("content-type".to_string(), "application/json".to_string()),
            ]),
            body: error_response,
            stream: None,
        })
    }
}

/// Create executor for the canonical provider id
pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(VeoAiFreeWebExecutor)
}

fn is_private_address(addr: &str) -> bool {
    let lower = addr.to_lowercase();
    let trimmed = lower.trim_start_matches('[').trim_end_matches(']');
    let mapped = if trimmed.starts_with("::ffff:") {
        &trimmed[7..]
    } else {
        trimmed
    };

    if mapped.contains('.') {
        let parts: Vec<&str> = mapped.split('.').collect();
        if parts.len() >= 2 {
            if let (Ok(o1), Ok(o2)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                return o1 == 0 || o1 == 10 || o1 == 127 || o1 >= 224 ||
                    (o1 == 172 && o2 >= 16 && o2 <= 31) ||
                    (o1 == 192 && o2 == 168) ||
                    (o1 == 169 && o2 == 254);
            }
        }
    }

    // IPv6 checks
    mapped == "::1" || mapped.starts_with("fc") || mapped.starts_with("fd") ||
        mapped.starts_with("fe80") || mapped.starts_with("fe9") ||
        mapped.starts_with("fea") || mapped.starts_with("feb")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_private_address() {
        assert!(is_private_address("127.0.0.1"));
        assert!(is_private_address("10.0.0.1"));
        assert!(is_private_address("192.168.1.1"));
        assert!(is_private_address("172.16.0.1"));
        assert!(is_private_address("::1"));
        assert!(!is_private_address("8.8.8.8"));
    }
}
