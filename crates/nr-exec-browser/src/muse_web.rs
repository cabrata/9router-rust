//! Port of open-sse/executors/muse-web.js + companions
//! muse.ai Hatch agent via Playwright
//! Auth: Cookie-based session renewal via /api/session
//! Response: Poll agent bubbles, handle approval cards, extract media

use nr_core::Executor;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

/// MuseWebExecutor — muse.ai agent via browser automation
pub struct MuseWebExecutor;

#[async_trait]
impl Executor for MuseWebExecutor {
    async fn execute(
        &self,
        _req: &nr_core::ExecutorRequest,
    ) -> Result<nr_core::ExecutorResponse, Box<dyn std::error::Error>> {
        // TODO: Implement Muse web executor
        // This requires:
        // 1. Session renewal via HTTP /api/session
        // 2. Browser context with rotating cookies
        // 3. Flatten messages to single prompt
        // 4. Poll agent bubbles every 300ms
        // 5. Handle approval cards (deny automatically)
        // 6. Extract media from replies
        // 7. Format as OpenAI SSE

        let error_response = json!({
            "error": {
                "message": "muse-web browser automation not yet implemented",
                "type": "internal_error"
            }
        });

        Ok(nr_core::ExecutorResponse {
            status: 503,
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
    Arc::new(MuseWebExecutor)
}

/// Parse cookie pairs from raw cookie header
fn parse_cookie_pairs(raw: &str) -> Vec<(String, String)> {
    raw.split(';')
        .map(|p| p.trim())
        .filter_map(|pair| {
            let eq_idx = pair.find('=')?;
            let name = pair[..eq_idx].trim();
            let value = pair[eq_idx + 1..].trim();
            if name.is_empty() || value.is_empty() {
                return None;
            }
            Some((name.to_string(), value.to_string()))
        })
        .collect()
}

/// Flatten messages to single prompt for muse
fn flatten_messages(messages: &[Value]) -> String {
    let user_msgs: Vec<_> = messages
        .iter()
        .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        .filter_map(|m| m.get("content").and_then(|c| c.as_str()))
        .collect();

    if user_msgs.len() == 1 {
        return user_msgs[0].to_string();
    }

    // Multi-turn: include context
    let history: Vec<String> = messages
        .iter()
        .filter_map(|m| {
            let role = m.get("role").and_then(|r| r.as_str())?;
            let content = m.get("content").and_then(|c| c.as_str())?;
            if content.trim().is_empty() {
                return None;
            }
            let role_label = if role == "user" { "User" } else { "Assistant" };
            Some(format!("{}: {}", role_label, content))
        })
        .collect();

    history.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cookie_pairs() {
        let raw = "name=value; foo=bar";
        let pairs = parse_cookie_pairs(raw);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0, "name");
        assert_eq!(pairs[0].1, "value");
    }

    #[test]
    fn test_flatten_messages_single() {
        let messages = vec![json!({"role": "user", "content": "Hello"})];
        let prompt = flatten_messages(&messages);
        assert_eq!(prompt, "Hello");
    }
}
