//! Port of open-sse/utils/geminiCookies.js.
//!
//! Normalizes a pasted Gemini cookie header, bare PSID, or browser-export JSON.

use serde_json::Value;

fn cookie_pairs_from_json(raw: &str) -> Vec<String> {
    if !raw.starts_with('{') {
        return Vec::new();
    }
    let Ok(parsed) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    let cookies = match parsed.get("cookies") {
        Some(c) if c.as_object().is_some() => c,
        _ => &parsed,
    };
    let Some(map) = cookies.as_object() else {
        return Vec::new();
    };
    let mut pairs = Vec::new();
    for (name, value) in map {
        let Some(v) = value.as_str() else { continue };
        let v = v.trim();
        if v.is_empty() {
            continue;
        }
        pairs.push(format!("{name}={v}"));
    }
    pairs
}

/// `normalizeGeminiCookieInput`. Default cookie name `__Secure-1PSID`.
pub fn normalize_gemini_cookie_input(raw: &str, cookie_name: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let json_pairs = cookie_pairs_from_json(trimmed);
    if !json_pairs.is_empty() {
        return json_pairs.join("; ");
    }
    if trimmed.contains('=') {
        trimmed.to_string()
    } else {
        format!("{cookie_name}={trimmed}")
    }
}

/// `normalizeGeminiValidationCookie` — normalize a pasted Gemini credential for the
/// connection test and make sure it actually carries a non-empty `__Secure-1PSID`
/// cookie. Strips a pasted `Cookie:` header prefix. Returns `None` when no PSID is
/// present so callers can reject before any network call.
pub fn normalize_gemini_validation_cookie(raw: &str) -> Option<String> {
    let stripped = regex::Regex::new(r"(?i)^cookie:\s*")
        .ok()
        .and_then(|re| re.replace(raw.trim(), "").into_owned().into())
        .unwrap_or_else(|| raw.trim().to_string());
    let cookie = normalize_gemini_cookie_input(&stripped, "__Secure-1PSID");
    let psid = cookie
        .split(';')
        .map(str::trim)
        .find(|part| part.to_lowercase().starts_with("__secure-1psid="))?;
    let value = psid[psid.find('=')? + 1..].trim();
    if value.is_empty() {
        return None;
    }
    Some(cookie)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_psid_gets_name() {
        assert_eq!(
            normalize_gemini_cookie_input("abc123", "__Secure-1PSID"),
            "__Secure-1PSID=abc123"
        );
    }

    #[test]
    fn header_passthrough() {
        assert_eq!(
            normalize_gemini_cookie_input("a=1; b=2", "__Secure-1PSID"),
            "a=1; b=2"
        );
    }

    #[test]
    fn browser_json() {
        let raw = r#"{"cookies":{"__Secure-1PSID":" abc ","other":""}}"#;
        assert_eq!(
            normalize_gemini_cookie_input(raw, "__Secure-1PSID"),
            "__Secure-1PSID=abc"
        );
        // A flat object is also accepted.
        let flat = r#"{"__Secure-1PSID":"xyz"}"#;
        assert_eq!(normalize_gemini_cookie_input(flat, "__Secure-1PSID"), "__Secure-1PSID=xyz");
    }

    #[test]
    fn validation_requires_nonempty_psid() {
        assert!(normalize_gemini_validation_cookie("abc123").is_some());
        assert!(normalize_gemini_validation_cookie("cookie: __Secure-1PSID=abc").is_some());
        assert_eq!(normalize_gemini_validation_cookie("__Secure-1PSID="), None);
        assert_eq!(normalize_gemini_validation_cookie("other=1"), None);
        assert_eq!(normalize_gemini_validation_cookie(""), None);
    }
}
