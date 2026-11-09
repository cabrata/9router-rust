//! Port of open-sse/utils/webCookieAuth.js.
//!
//! Cookie-input normalization for web-cookie providers. Accepts whatever the user
//! pasted (bare value, `name=value`, full DevTools blob, or a cookie-editor JSON
//! array) and produces a `Cookie` header string.

use serde_json::Value;

/// Cookie attributes that appear in a raw blob but are not name=value pairs.
const COOKIE_ATTRIBUTE_NAMES: &[&str] = &[
    "path", "domain", "expires", "max-age", "secure", "httponly", "samesite",
];

/// `stripCookieInputPrefix` — drop a leading `Bearer ` / `Cookie:` from pasted input.
pub fn strip_cookie_input_prefix(raw_value: &str) -> String {
    let trimmed = raw_value.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let without_bearer = trimmed
        .strip_prefix("bearer ")
        .or_else(|| trimmed.strip_prefix("Bearer "))
        .unwrap_or(trimmed);
    without_bearer
        .strip_prefix("cookie:")
        .or_else(|| without_bearer.strip_prefix("Cookie:"))
        .unwrap_or(without_bearer)
        .trim()
        .to_string()
}

/// `parseJsonCookiesToHeader`. Returns:
/// - `None` when the input is not a JSON array (pass-through),
/// - `Some("")` for an empty array,
/// - `Err(message)` on a JSON entry missing the required `name` or `value` field.
pub fn parse_json_cookies_to_header(raw_value: &str) -> Option<Result<String, String>> {
    let trimmed = raw_value.trim();
    if trimmed.is_empty() || !trimmed.starts_with('[') {
        return None;
    }
    let Ok(parsed) = serde_json::from_str::<Value>(trimmed) else {
        return Some(Err("invalid cookie JSON".to_string()));
    };
    let Value::Array(items) = parsed else {
        return None;
    };
    if items.is_empty() {
        return Some(Ok(String::new()));
    }

    let mut parts = Vec::with_capacity(items.len());
    for (i, entry) in items.iter().enumerate() {
        let Some(obj) = entry.as_object() else {
            return Some(Err(format!(
                "Invalid cookie JSON at index {i}: expected an object"
            )));
        };
        let Some(name) = obj.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()) else {
            return Some(Err(format!(
                "Invalid cookie JSON at index {i}: missing required field 'name'"
            )));
        };
        let Some(value) = obj.get("value").and_then(Value::as_str) else {
            return Some(Err(format!(
                "Invalid cookie JSON at index {i}: missing required field 'value'"
            )));
        };
        parts.push(format!("{name}={value}"));
    }
    Some(Ok(parts.join("; ")))
}

/// `normalizeSessionCookieHeader`.
pub fn normalize_session_cookie_header(raw_value: &str, default_cookie_name: &str) -> String {
    let stripped = strip_cookie_input_prefix(raw_value);
    if stripped.is_empty() {
        return String::new();
    }
    if let Some(json_result) = parse_json_cookies_to_header(&stripped) {
        return json_result.unwrap_or_default();
    }
    if stripped.contains('=') {
        return stripped;
    }
    format!("{default_cookie_name}={stripped}")
}

/// `normalizeSessionCookieHeaders` — normalize many, dropping empties and duplicates.
pub fn normalize_session_cookie_headers<'a, I>(raw_values: I, default_cookie_name: &str) -> Vec<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for raw in raw_values {
        let normalized = normalize_session_cookie_header(raw, default_cookie_name);
        if normalized.is_empty() || !seen.insert(normalized.clone()) {
            continue;
        }
        out.push(normalized);
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PlaywrightCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
}

/// `toPlaywrightCookies`. `__Host-` cookies must be host-only (no Domain attribute) —
/// Chrome rejects the whole batch with "Invalid cookie fields" when one carries a
/// domain — and the host must be concrete (no leading dot). Everything else gets
/// `domain`.
pub fn to_playwright_cookies(raw_value: &str, default_domain: &str) -> Vec<PlaywrightCookie> {
    let bare_domain = default_domain.strip_prefix('.').unwrap_or(default_domain);
    let mut entries = Vec::new();
    for part in raw_value.split(';') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some(eq) = trimmed.find('=') else { continue };
        if eq == 0 {
            continue;
        }
        let name = trimmed[..eq].trim();
        let value = trimmed[eq + 1..].trim();
        if name.is_empty() || value.is_empty() {
            continue;
        }
        if COOKIE_ATTRIBUTE_NAMES.iter().any(|a| a.eq_ignore_ascii_case(name)) {
            continue;
        }
        if name.starts_with("__Host-") {
            entries.push(PlaywrightCookie {
                name: name.to_string(),
                value: value.to_string(),
                domain: bare_domain.to_string(),
                path: "/".to_string(),
                secure: true,
            });
        } else {
            entries.push(PlaywrightCookie {
                name: name.to_string(),
                value: value.to_string(),
                domain: default_domain.to_string(),
                path: "/".to_string(),
                secure: true,
            });
        }
    }
    entries
}

/// `extractCookieValue` — pull a single cookie's value out of pasted input.
///
///   - bare value:                "eyJ0eXAi..."              → "eyJ0eXAi..."
///   - single pair:               "sso=eyJ0eXAi..."          → "eyJ0eXAi..."
///   - full DevTools cookie blob: "foo=1; sso=eyJ...; bar=2" → "eyJ..."
///
/// Returns `""` when a blob is given that does not contain the named cookie.
pub fn extract_cookie_value(raw_value: &str, cookie_name: &str) -> String {
    let trimmed = strip_cookie_input_prefix(raw_value);
    if trimmed.is_empty() {
        return String::new();
    }

    if trimmed.contains(';') {
        let escaped = regex::escape(cookie_name);
        // (?:^|;\s*)<name>=([^;\s]+)
        let pattern = format!(r#"(?:^|;\s*){escaped}=([^;\s]+)"#);
        if let Ok(re) = regex::Regex::new(&pattern) {
            if let Some(m) = re.captures(&trimmed) {
                return m[1].to_string();
            }
            return String::new();
        }
        // Unreachable for a controlled pattern; fall through to prefix matching.
    }

    let prefix = format!("{cookie_name}=");
    if let Some(rest) = trimmed.strip_prefix(&prefix) {
        return rest.to_string();
    }
    trimmed
}

/// `buildGrokCookieHeader` — always emits `sso=<value>`; forwards the paired `sso-rw`
/// write cookie plus Grok's Cloudflare cookies (`cf_clearance`, `__cf_bm`) when they
/// appear as real cookie pairs. Grok's anti-bot rejects requests that send `sso`
/// without `sso-rw`.
pub fn build_grok_cookie_header(raw_value: &str) -> String {
    let sso = extract_cookie_value(raw_value, "sso");
    if sso.is_empty() {
        return String::new();
    }
    let mut parts = vec![format!("sso={sso}")];
    for name in ["sso-rw", "cf_clearance", "__cf_bm"] {
        let prefix = format!("{}=", name);
        let present = raw_value.split(';').any(|part| {
            let p = part.trim();
            p.starts_with(&prefix) || p == name
        }) || raw_value.split("; ").any(|p| p.starts_with(&prefix));
        if !present {
            continue;
        }
        let value = extract_cookie_value(raw_value, name);
        if !value.is_empty() {
            parts.push(format!("{name}={value}"));
        }
    }
    parts.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_prefixes() {
        assert_eq!(strip_cookie_input_prefix("Cookie: a=b"), "a=b");
        assert_eq!(strip_cookie_input_prefix("bearer abc"), "abc");
        assert_eq!(strip_cookie_input_prefix("  sso=x  "), "sso=x");
        assert_eq!(strip_cookie_input_prefix(""), "");
    }

    #[test]
    fn normalizes_bare_and_pair() {
        assert_eq!(normalize_session_cookie_header("tok123", "sso"), "sso=tok123");
        assert_eq!(normalize_session_cookie_header("sso=tok123", "sso"), "sso=tok123");
        assert_eq!(normalize_session_cookie_header("", "sso"), "");
    }

    #[test]
    fn parses_cookie_editor_json() {
        let json = r#"[{"name":"sso","value":"v1","domain":".x.com"},{"name":"b","value":"v2"}]"#;
        assert_eq!(
            normalize_session_cookie_header(json, "sso"),
            "sso=v1; b=v2"
        );
    }

    #[test]
    fn json_entry_missing_name_errors() {
        let json = r#"[{"value":"v1"}]"#;
        assert_eq!(
            parse_json_cookies_to_header(json),
            Some(Err("Invalid cookie JSON at index 0: missing required field 'name'".to_string()))
        );
    }

    #[test]
    fn extract_from_blob() {
        assert_eq!(
            extract_cookie_value("foo=1; sso=eyJabc; bar=2", "sso"),
            "eyJabc"
        );
        assert_eq!(extract_cookie_value("sso=eyJabc", "sso"), "eyJabc");
        assert_eq!(extract_cookie_value("eyJabc", "sso"), "eyJabc");
        assert_eq!(extract_cookie_value("foo=1; bar=2", "sso"), "");
    }

    #[test]
    fn grok_header() {
        assert_eq!(build_grok_cookie_header("ssoabc"), "sso=ssoabc");
        assert_eq!(build_grok_cookie_header("foo=1; sso=abc"), "sso=abc");
        assert_eq!(
            build_grok_cookie_header("foo=1; sso=abc; sso-rw=rw1"),
            "sso=abc; sso-rw=rw1"
        );
        // bare sso without the pair sends exactly sso=<value>
        assert_eq!(build_grok_cookie_header("sso=abc"), "sso=abc");
        assert_eq!(build_grok_cookie_header(""), "");
    }

    #[test]
    fn playwright_host_cookies() {
        let cookies = to_playwright_cookies("__Host-a=1; Path=/; sso=2", "example.com");
        assert_eq!(cookies.len(), 2);
        assert_eq!(cookies[0].domain, "example.com");
        assert_eq!(cookies[1].domain, "example.com");
        assert_eq!(cookies[1].name, "sso");
        let dot = to_playwright_cookies("__Host-a=1", ".example.com");
        assert_eq!(dot[0].domain, "example.com");
    }
}
