//! Rewrite Antigravity IDE identity markers so upstream AG 2.x accepts them.
//! Port of src/mitm/antigravityIdeVersion.js.
//!
//! Only generation requests (`:generateContent` / `:streamGenerateContent`) are
//! rewritten; catalog and other passthrough requests keep the client's identity.

use http::HeaderMap;
use serde_json::Value;

pub const ANTIGRAVITY_IDE_VERSION: &str = "2.24.0";
const ANTIGRAVITY_IDE_VERSION_OVERRIDE_ENABLED: bool = true;

fn should_rewrite_metadata(metadata: &Value) -> bool {
    let Some(obj) = metadata.as_object() else {
        return false;
    };
    if obj
        .get("ideName")
        .and_then(|v| v.as_str())
        .is_some_and(|s| s.to_ascii_lowercase() == "antigravity")
    {
        return true;
    }
    if obj
        .get("ideType")
        .and_then(|v| v.as_str())
        .is_some_and(|s| s.to_ascii_uppercase() == "ANTIGRAVITY")
    {
        return true;
    }
    obj.contains_key("ideVersion")
}

pub fn rewrite_antigravity_user_agent(user_agent: &str, version: &str) -> String {
    if !user_agent.contains("antigravity/") {
        return user_agent.to_string();
    }
    // Replace `antigravity/<token>` wherever it appears, matching the JS regex
    // `/antigravity\/[^\s]+/` (first occurrence semantics differ only in degenerate
    // multi-token UAs, which AG never sends).
    match user_agent.find("antigravity/") {
        Some(idx) => {
            let rest = &user_agent[idx + "antigravity/".len()..];
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            format!("{}antigravity/{version}{}", &user_agent[..idx], &rest[end..])
        }
        None => user_agent.to_string(),
    }
}

/// Result of the override: possibly-rewritten body and headers.
pub struct VersionOverride {
    pub body: Vec<u8>,
    pub headers: HeaderMap,
    pub applied: bool,
}

fn is_generation_endpoint(request_url: &str) -> bool {
    request_url.contains(":generateContent") || request_url.contains(":streamGenerateContent")
}

/// Rewrite the IDE version + user-agent on Antigravity generation requests.
///
/// Returns the input untouched when the URL isn't a generation endpoint, the
/// feature flag is off, or the body isn't the JSON object shape we can rewrite.
pub fn apply_antigravity_ide_version_override(
    body: &[u8],
    headers: &HeaderMap,
    request_url: &str,
) -> VersionOverride {
    if !ANTIGRAVITY_IDE_VERSION_OVERRIDE_ENABLED || !is_generation_endpoint(request_url) {
        return VersionOverride {
            body: body.to_vec(),
            headers: headers.clone(),
            applied: false,
        };
    }

    let mut next_headers = headers.clone();
    let user_agent = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let next_ua = rewrite_antigravity_user_agent(user_agent, ANTIGRAVITY_IDE_VERSION);
    let ua_changed = next_ua != user_agent;
    if ua_changed {
        if let Ok(v) = http::HeaderValue::from_str(&next_ua) {
            next_headers.insert("user-agent", v);
        }
    }

    let Ok(mut parsed) = serde_json::from_slice::<Value>(body) else {
        return VersionOverride {
            body: body.to_vec(),
            headers: next_headers,
            applied: ua_changed,
        };
    };

    let should = parsed.get("metadata").is_some_and(should_rewrite_metadata);
    if !should {
        return VersionOverride {
            body: body.to_vec(),
            headers: next_headers,
            applied: ua_changed,
        };
    }

    if let Some(meta) = parsed.get_mut("metadata").and_then(|m| m.as_object_mut()) {
        meta.insert(
            "ideVersion".into(),
            Value::String(ANTIGRAVITY_IDE_VERSION.into()),
        );
    }
    let next_body = serde_json::to_vec(&parsed).unwrap_or_else(|_| body.to_vec());
    VersionOverride {
        body: next_body,
        headers: next_headers,
        applied: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(ua: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("user-agent", http::HeaderValue::from_str(ua).unwrap());
        h
    }

    #[test]
    fn rewrites_version_in_user_agent() {
        assert_eq!(
            rewrite_antigravity_user_agent(
                "antigravity/1.9.0 linux/x64",
                ANTIGRAVITY_IDE_VERSION
            ),
            "antigravity/2.24.0 linux/x64"
        );
        assert_eq!(
            rewrite_antigravity_user_agent("curl/8.0", ANTIGRAVITY_IDE_VERSION),
            "curl/8.0"
        );
    }

    #[test]
    fn rewrites_metadata_on_generation_endpoint() {
        let body = br#"{"model":"x","metadata":{"ideName":"antigravity","ideVersion":"1.0.0"}}"#;
        let out = apply_antigravity_ide_version_override(
            body,
            &hdr("antigravity/1.9.0 linux"),
            "/v1/models/x:streamGenerateContent",
        );
        assert!(out.applied);
        let v: Value = serde_json::from_slice(&out.body).unwrap();
        assert_eq!(v["metadata"]["ideVersion"], ANTIGRAVITY_IDE_VERSION);
        assert_eq!(
            out.headers.get("user-agent").unwrap(),
            "antigravity/2.24.0 linux"
        );
    }

    #[test]
    fn leaves_non_generation_endpoints_untouched() {
        let body = br#"{"metadata":{"ideName":"antigravity"}}"#;
        let out = apply_antigravity_ide_version_override(
            body,
            &hdr("antigravity/1.9.0"),
            "/v1/models",
        );
        assert!(!out.applied);
        assert_eq!(out.body, body);
        assert_eq!(out.headers.get("user-agent").unwrap(), "antigravity/1.9.0");
    }

    #[test]
    fn ignores_bodies_without_ag_metadata() {
        let body = br#"{"model":"x","metadata":{"ideName":"vscode"}}"#;
        let out = apply_antigravity_ide_version_override(
            body,
            &hdr("x"),
            "/:generateContent",
        );
        assert!(!out.applied);
        assert_eq!(out.body, body);
    }
}
