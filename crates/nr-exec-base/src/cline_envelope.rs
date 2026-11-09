//! Port of open-sse/shared/clineEnvelope.js — Unwrap Cline's non-stream response envelope.

use nr_core::Json;

/// Unwrap Cline's non-stream envelope: {"success":true,"data":{...choices...}}.
///
/// Scoped to providers opting in via `transport.quirks.clineEnvelope` so no other
/// provider's body is ever rewritten. The error envelope ({"success":false,...})
/// never matches and passes through untouched.
pub fn unwrap_cline_envelope(body: &Json, provider: &str) -> Json {
    // Check if provider has quirks.clineEnvelope set
    let config = nr_data::provider(provider).unwrap_or(&Json::Null);
    let has_cline_envelope = config
        .get("quirks")
        .and_then(|q| q.get("clineEnvelope"))
        .and_then(Json::as_bool)
        .unwrap_or(false);

    if !has_cline_envelope {
        return body.clone();
    }

    // Check for success=true and data object
    if let Some(obj) = body.as_object() {
        if let Some(success) = obj.get("success").and_then(Json::as_bool) {
            if success {
                if let Some(data) = obj.get("data") {
                    if data.is_object() {
                        return data.clone();
                    }
                }
            }
        }
    }

    body.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_unwrap_cline_envelope_success() {
        let body = json!({"success": true, "data": {"id": "123", "object": "chat.completion"}});
        let provider = "unknown-provider-no-quirk";
        let result = unwrap_cline_envelope(&body, provider);
        // If quirk is not set, body is returned unchanged
        assert_eq!(result, body);
    }

    #[test]
    fn test_unwrap_cline_envelope_error() {
        let body = json!({"success": false, "error": "Something went wrong"});
        let result = unwrap_cline_envelope(&body, "cline");
        assert_eq!(result, body);
    }

    #[test]
    fn test_unwrap_cline_envelope_no_data() {
        let body = json!({"success": true});
        let result = unwrap_cline_envelope(&body, "cline");
        // No data field, return unchanged
        assert_eq!(result, body);
    }
}
