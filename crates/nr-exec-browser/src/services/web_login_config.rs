//! Port of open-sse/services/webLoginConfig.js
//! Per-provider login configuration for web-cookie auth flows

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Token source for credential harvesting
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenSource {
    pub source_type: String, // "cookie" or "localStorage"
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub optional: Option<bool>,
}

/// Configuration for web login flow
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebLoginConfig {
    pub login_url: String,
    pub cookie_domain: String,
    pub token_sources: Vec<TokenSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_from: Option<String>, // "cookieHeader" or "localStorageValue"
}

impl WebLoginConfig {
    /// Get configuration for grok-web
    pub fn grok_web() -> Self {
        Self {
            login_url: "https://grok.com/auth/login".to_string(),
            cookie_domain: ".grok.com".to_string(),
            token_sources: vec![TokenSource {
                source_type: "cookie".to_string(),
                name: "sso".to_string(),
                optional: None,
            }],
            credential_from: Some("cookieHeader".to_string()),
        }
    }

    /// Get configuration for gemini-web
    pub fn gemini_web() -> Self {
        Self {
            login_url: "https://gemini.google.com/app".to_string(),
            cookie_domain: ".google.com".to_string(),
            token_sources: vec![
                TokenSource {
                    source_type: "cookie".to_string(),
                    name: "__Secure-1PSID".to_string(),
                    optional: None,
                },
                TokenSource {
                    source_type: "cookie".to_string(),
                    name: "__Secure-1PSIDTS".to_string(),
                    optional: Some(true),
                },
            ],
            credential_from: Some("cookieHeader".to_string()),
        }
    }

    /// Get configuration for muse-web
    pub fn muse_web() -> Self {
        Self {
            login_url: "https://muse.ai".to_string(),
            cookie_domain: ".muse.ai".to_string(),
            token_sources: vec![TokenSource {
                source_type: "cookie".to_string(),
                name: "hatch_sess".to_string(),
                optional: None,
            }],
            credential_from: Some("cookieHeader".to_string()),
        }
    }

    /// Get configuration for chatgpt-web
    pub fn chatgpt_web() -> Self {
        Self {
            login_url: "https://chatgpt.com".to_string(),
            cookie_domain: ".chatgpt.com".to_string(),
            token_sources: vec![TokenSource {
                source_type: "cookie".to_string(),
                name: "__Secure-next-auth.session-token".to_string(),
                optional: None,
            }],
            credential_from: Some("cookieHeader".to_string()),
        }
    }

    /// Get configuration by provider name
    pub fn for_provider(provider: &str) -> Option<Self> {
        match provider {
            "grok-web" => Some(Self::grok_web()),
            "gemini-web" => Some(Self::gemini_web()),
            "muse-web" => Some(Self::muse_web()),
            "chatgpt-web" => Some(Self::chatgpt_web()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grok_web_config() {
        let cfg = WebLoginConfig::grok_web();
        assert_eq!(cfg.cookie_domain, ".grok.com");
        assert_eq!(cfg.token_sources[0].name, "sso");
    }

    #[test]
    fn test_gemini_web_config() {
        let cfg = WebLoginConfig::gemini_web();
        assert_eq!(cfg.cookie_domain, ".google.com");
        assert_eq!(cfg.token_sources[0].name, "__Secure-1PSID");
    }

    #[test]
    fn test_for_provider() {
        assert!(WebLoginConfig::for_provider("grok-web").is_some());
        assert!(WebLoginConfig::for_provider("unknown").is_none());
    }
}
