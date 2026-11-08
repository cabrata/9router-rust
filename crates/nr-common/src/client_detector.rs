//! Detect CLI tool identity from request headers/body.

use serde_json::Value;
use std::collections::HashMap;

const NATIVE_PAIRS: &[(&str, &[&str])] = &[
    ("claude", &["claude", "anthropic"]),
    ("gemini-cli", &["gemini-cli"]),
    ("antigravity", &["antigravity"]),
    ("codex", &["codex"]),
];

/// Detect which CLI tool is making the request
pub fn detect_client_tool(
    headers: Option<&HashMap<String, String>>,
    body: Option<&Value>,
) -> Option<&'static str> {
    let ua = headers.and_then(|h| h.get("user-agent")).map(|s| s.to_lowercase()).unwrap_or_default();
    let x_app = headers.and_then(|h| h.get("x-app")).map(|s| s.to_lowercase()).unwrap_or_default();
    let openai_intent = headers.and_then(|h| h.get("openai-intent")).map(|s| s.to_lowercase()).unwrap_or_default();
    let initiator = headers.and_then(|h| h.get("x-initiator").or_else(|| h.get("X-Initiator"))).map(|s| s.to_lowercase()).unwrap_or_default();
    let originator = headers.and_then(|h| h.get("originator")).map(|s| s.to_lowercase()).unwrap_or_default();

    // Antigravity: detected via body field
    if body.and_then(|b| b.get("userAgent")).and_then(Value::as_str).map(|s| s == "antigravity").unwrap_or(false) {
        return Some("antigravity");
    }

    // GitHub Copilot
    if ua.contains("githubcopilotchat") || openai_intent == "conversation-panel" || initiator == "user" {
        return Some("github-copilot");
    }

    // Claude Code / Claude CLI
    if ua.contains("claude-cli") || ua.contains("claude-code") || x_app == "cli" {
        return Some("claude");
    }

    // Gemini CLI
    if ua.contains("gemini-cli") {
        return Some("gemini-cli");
    }

    // Codex
    if ua.contains("codex-tui") || ua.contains("codex-cli") || ua.contains("codex_cli_rs") ||
        ua.contains("codex desktop") || originator.starts_with("codex_") {
        return Some("codex");
    }

    // DeepSeek TUI
    if ua.contains("deepseek-tui") {
        return Some("deepseek-tui");
    }

    None
}

/// Check if CLI tool + provider pair should be passed through losslessly
pub fn is_native_passthrough(client_tool: Option<&str>, provider: &str) -> bool {
    if let Some(tool) = client_tool {
        for (native_tool, native_providers) in NATIVE_PAIRS {
            if *native_tool == tool {
                let normalized = if provider.starts_with("anthropic-compatible") {
                    "anthropic"
                } else {
                    provider
                };
                return native_providers.contains(&normalized);
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_claude_cli() {
        let mut headers = HashMap::new();
        headers.insert("user-agent".to_string(), "claude-cli/2.1.0".to_string());
        assert_eq!(detect_client_tool(Some(&headers), None), Some("claude"));
    }

    #[test]
    fn test_detect_codex() {
        let mut headers = HashMap::new();
        headers.insert("user-agent".to_string(), "codex-tui/1.0".to_string());
        assert_eq!(detect_client_tool(Some(&headers), None), Some("codex"));
    }

    #[test]
    fn test_is_native_passthrough() {
        assert!(is_native_passthrough(Some("claude"), "claude"));
        assert!(is_native_passthrough(Some("claude"), "anthropic"));
        assert!(is_native_passthrough(Some("claude"), "anthropic-compatible-chat-xyz"));
        assert!(!is_native_passthrough(Some("claude"), "openai"));
    }
}
