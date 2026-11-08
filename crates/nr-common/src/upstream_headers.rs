//! Extract rate-limit and retry headers from upstream responses

use std::collections::HashMap;

const FORWARDED: &[&str] = &["retry-after", "x-should-retry"];
const FORWARDED_PREFIX: &str = "anthropic-ratelimit-";

/// Extract upstream response headers to forward to client
pub fn upstream_response_headers(headers: &HashMap<String, String>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (name, value) in headers {
        let key = name.to_lowercase();
        if FORWARDED.contains(&key.as_str()) || key.starts_with(FORWARDED_PREFIX) {
            out.insert(key, value.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_upstream_response_headers_forwards_rate_limit() {
        let mut headers = HashMap::new();
        headers.insert("Anthropic-RateLimit-Requests-Limit".to_string(), "100".to_string());
        headers.insert("X-Custom".to_string(), "skip-me".to_string());
        
        let result = upstream_response_headers(&headers);
        assert!(result.contains_key("anthropic-ratelimit-requests-limit"));
        assert!(!result.contains_key("x-custom"));
    }

    #[test]
    fn test_upstream_response_headers_forwards_retry_after() {
        let mut headers = HashMap::new();
        headers.insert("Retry-After".to_string(), "60".to_string());
        
        let result = upstream_response_headers(&headers);
        assert_eq!(result.get("retry-after"), Some(&"60".to_string()));
    }
}
