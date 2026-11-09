//! Port of open-sse/shared/clineAuth.js — Cline OAuth header building.

use std::collections::HashMap;

/// Resolve Cline access token from raw token, prefixing WorkOS JWTs.
pub fn get_cline_access_token(token: &str) -> String {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.to_lowercase().starts_with("workos:") {
        return trimmed.to_string();
    }
    // Cline OAuth access tokens are WorkOS JWTs (base64url `eyJ…` header).
    // ClinePass API keys (category "apikey", e.g. `clp_…`) are NOT JWTs and must
    // be sent verbatim — prefixing them with `workos:` makes the Cline API reject
    // the request with HTTP 401.
    let is_workos_jwt = trimmed.starts_with("eyJ") && trimmed.len() > 50;
    if is_workos_jwt {
        format!("workos:{}", trimmed)
    } else {
        trimmed.to_string()
    }
}

/// Build Cline-specific HTTP headers from token.
pub fn build_cline_headers(token: &str) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    headers.insert("HTTP-Referer".to_string(), "https://cline.bot".to_string());
    headers.insert("X-Title".to_string(), "Cline".to_string());

    let version = nr_data::export("open-sse/config/appConstants", "APP_VERSION")
        .and_then(|v| v.as_str())
        .unwrap_or("0.0.0");
    headers.insert("User-Agent".to_string(), format!("9Router/{}", version));

    headers.insert("X-PLATFORM".to_string(), std::env::consts::OS.to_string());
    headers.insert("X-PLATFORM-VERSION".to_string(), "unknown".to_string());
    headers.insert("X-CLIENT-TYPE".to_string(), "9router".to_string());
    headers.insert("X-CLIENT-VERSION".to_string(), version.to_string());
    headers.insert("X-CORE-VERSION".to_string(), version.to_string());
    headers.insert("X-IS-MULTIROOT".to_string(), "false".to_string());

    let access_token = get_cline_access_token(token);
    if !access_token.is_empty() {
        headers.insert("Authorization".to_string(), format!("Bearer {}", access_token));
    }

    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_cline_access_token_workos_jwt() {
        // JWT-like token starting with eyJ...
        let token = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let result = get_cline_access_token(token);
        assert!(result.starts_with("workos:"));
        assert!(result.contains("eyJ"));
    }

    #[test]
    fn test_get_cline_access_token_api_key() {
        let token = "clp_1234567890abcdef";
        let result = get_cline_access_token(token);
        assert_eq!(result, token);
        assert!(!result.starts_with("workos:"));
    }

    #[test]
    fn test_get_cline_access_token_workos_prefixed() {
        let token = "workos:eyJhbGciOiJIUzI1NiJ9.payload.sig";
        let result = get_cline_access_token(token);
        assert_eq!(result, token);
    }

    #[test]
    fn test_get_cline_access_token_empty() {
        assert_eq!(get_cline_access_token(""), "");
        assert_eq!(get_cline_access_token("   "), "");
    }

    #[test]
    fn test_build_cline_headers() {
        let token = "clp_test";
        let headers = build_cline_headers(token);
        assert_eq!(headers.get("HTTP-Referer").map(|s| s.as_str()), Some("https://cline.bot"));
        assert_eq!(headers.get("X-Title").map(|s| s.as_str()), Some("Cline"));
        assert_eq!(headers.get("X-CLIENT-TYPE").map(|s| s.as_str()), Some("9router"));
        assert!(headers.get("User-Agent").map(|s| s.contains("9Router")).unwrap_or(false));
    }
}
