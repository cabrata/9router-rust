//! Strip request params a provider/model rejects. Port of open-sse/translator/concerns/paramSupport.js.
//! TODO(dedupe): full rule implementation with getCapabilitiesForModel integration

use nr_core::Json;

/// Strip unsupported params from body in place; returns body.
/// TODO: full implementation with STRIP_RULES config-driven param removal
pub fn strip_unsupported_params(_provider: &str, _model: &str, body: &mut Json) -> &mut Json {
    if !body.is_object() {
        return body;
    }

    // Stub: rules from paramSupport.js need to be implemented here
    // - Claude models: drop temperature
    // - GitHub Copilot gpt-5.4: drop temperature
    // - GitHub Copilot Claude: drop thinking/reasoning_effort
    // - Cloudflare: flatten content
    // - etc.

    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_unsupported_params_noop() {
        let mut body = serde_json::json!({"model": "gpt-4", "temperature": 0.7});
        let result = strip_unsupported_params("openai", "gpt-4", &mut body);
        assert!(result.is_object());
    }
}
