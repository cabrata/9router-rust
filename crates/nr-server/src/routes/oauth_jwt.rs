//! JWT helpers shared by the OAuth import routes.
//!
//! Port of the JWT helpers in `src/lib/oauth/providerHelpers.js`
//! (`decodeJwtPayload`, `extractEmailFromAccessToken`, `extractCodexAccountInfo`).
//! We decode only; never verify — these tokens come from the provider and were
//! already validated by the token endpoint.

use base64::Engine;
use serde_json::{json, Value};

fn b64_url_decode(input: &str) -> Option<Vec<u8>> {
    // `replace(/-/g, "+").replace(/_/g, "/")` then pad to a multiple of 4.
    let normalised: String = input
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            other => other,
        })
        .collect();
    let missing = (4 - normalised.len() % 4) % 4;
    let padded = format!("{normalised}{}", "=".repeat(missing));
    base64::engine::general_purpose::STANDARD.decode(padded).ok()
}

/// `decodeJwtPayload(jwt)`: the middle segment as JSON, or `None`.
pub fn decode_jwt_payload(jwt: &str) -> Option<Value> {
    if jwt.is_empty() {
        return None;
    }
    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let bytes = b64_url_decode(parts[1])?;
    serde_json::from_slice(&bytes).ok()
}

/// `extractEmailFromAccessToken(accessToken)`: `email`, then
/// `preferred_username`, then `sub`.
pub fn extract_email_from_access_token(access_token: &str) -> Option<String> {
    let payload = decode_jwt_payload(access_token)?;
    for key in ["email", "preferred_username", "sub"] {
        if let Some(value) = payload.get(key).and_then(|v| v.as_str()) {
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// `extractCodexAccountInfo(idToken)` → `{ email, chatgptAccountId, chatgptPlanType }`.
///
/// Email is the bare `payload.email` only. The profile /
/// `preferred_username` fallbacks belong to the `codex/import-token` route's own
/// inline decode, not to this helper.
///
/// Claims live under the OpenAI auth namespace, with bare fallbacks:
/// `chatgpt_account_id` / `account_id`, `chatgpt_plan_type` / `plan_type`.
pub fn extract_codex_account_info(token: &str) -> Value {
    let Some(payload) = decode_jwt_payload(token) else {
        return json!({});
    };

    let chatgpt = payload
        .get("https://api.openai.com/auth")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let email = payload
        .get("email")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());

    let account_id = chatgpt
        .get("chatgpt_account_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            payload
                .get("account_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        });

    let plan_type = chatgpt
        .get("chatgpt_plan_type")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            payload
                .get("plan_type")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        });

    let mut out = serde_json::Map::new();
    if let Some(email) = email {
        out.insert("email".into(), json!(email));
    }
    if let Some(id) = account_id {
        out.insert("chatgptAccountId".into(), json!(id));
    }
    if let Some(plan) = plan_type {
        out.insert("chatgptPlanType".into(), json!(plan));
    }
    out.insert("jwtExp".into(), payload.get("exp").cloned().unwrap_or(Value::Null));
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_jwt(payload: Value) -> String {
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        format!("header.{body}.signature")
    }

    #[test]
    fn decodes_url_safe_payload() {
        let jwt = make_jwt(json!({ "email": "a@b.c" }));
        let payload = decode_jwt_payload(&jwt).expect("payload");
        assert_eq!(payload["email"], json!("a@b.c"));
    }

    #[test]
    fn rejects_malformed_tokens() {
        assert!(decode_jwt_payload("").is_none());
        assert!(decode_jwt_payload("only.two").is_none());
        assert!(decode_jwt_payload("not.a.jwt").is_none());
    }

    #[test]
    fn email_falls_back_through_claims() {
        assert_eq!(
            extract_email_from_access_token(&make_jwt(json!({ "email": "e@x" }))).as_deref(),
            Some("e@x")
        );
        assert_eq!(
            extract_email_from_access_token(&make_jwt(json!({ "preferred_username": "user" })))
                .as_deref(),
            Some("user")
        );
        assert_eq!(
            extract_email_from_access_token(&make_jwt(json!({ "sub": "sub-1" }))).as_deref(),
            Some("sub-1")
        );
        assert!(extract_email_from_access_token(&make_jwt(json!({}))).is_none());
    }

    #[test]
    fn codex_claims_prefer_namespaced_auth() {
        let info = extract_codex_account_info(&make_jwt(json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-1",
                "chatgpt_plan_type": "plus"
            },
            "https://api.openai.com/profile": { "email": "p@x" },
            "email": "bare@x",
            "exp": 1234
        })));
        // JS reads the bare `payload.email`; the profile object is ignored here.
        assert_eq!(info["email"], json!("bare@x"));
        assert_eq!(info["chatgptAccountId"], json!("acct-1"));
        assert_eq!(info["chatgptPlanType"], json!("plus"));
        assert_eq!(info["jwtExp"], json!(1234));
    }

    #[test]
    fn codex_email_ignores_profile_and_username() {
        // Only `payload.email` feeds this helper in the JS build.
        let info = extract_codex_account_info(&make_jwt(json!({
            "https://api.openai.com/profile": { "email": "p@x" },
            "preferred_username": "user"
        })));
        assert!(info.get("email").is_none(), "got {info}");
    }

    #[test]
    fn codex_claims_fall_back_to_bare_keys() {
        let info = extract_codex_account_info(&make_jwt(json!({
            "email": "bare@x",
            "account_id": "acct-2",
            "plan_type": "pro"
        })));
        assert_eq!(info["email"], json!("bare@x"));
        assert_eq!(info["chatgptAccountId"], json!("acct-2"));
        assert_eq!(info["chatgptPlanType"], json!("pro"));
    }

    #[test]
    fn garbage_token_yields_empty_object() {
        assert_eq!(extract_codex_account_info("nope"), json!({}));
    }
}
