use nr_core::{Json, JsonMap};
use std::sync::LazyLock;

/// Get capabilities for a model from the multi-layer fallback.
pub fn get_capabilities_for_model(provider: Option<&str>, model: Option<&str>) -> Json {
    let model = match model {
        Some(m) => m,
        None => return default_capabilities(),
    };

    let provider = provider.unwrap_or("");

    // 1. PROVIDER_CAPABILITIES[provider][model]
    if let Some(prov_caps) = crate::export("open-sse/providers/capabilities", "PROVIDER_CAPABILITIES")
        .and_then(|pc| pc.get(provider))
        .and_then(|p| p.get(model))
    {
        return merge_over_defaults(prov_caps);
    }

    // 2. MODEL_CAPABILITIES[model] — canonical exact id
    if let Some(model_caps) = crate::export("open-sse/providers/capabilities", "MODEL_CAPABILITIES")
        .and_then(|mc| mc.get(model))
    {
        return merge_over_defaults(model_caps);
    }

    // 3. PATTERN_CAPABILITIES (glob match)
    // TODO(dedupe): use pattern matching once available

    // 4. DEFAULT_CAPABILITIES
    default_capabilities()
}

/// Get capabilities for a custom model added via dashboard.
pub fn get_custom_model_capabilities(model_id: Option<&str>) -> Option<Json> {
    // TODO: integrate with custom model registry once persisted
    None
}

/// Get capabilities based on service kind
pub fn capabilities_from_service_kind(kind: Option<&str>) -> Option<Json> {
    let kind = kind?;
    match kind {
        "imageToText" => Some(serde_json::json!({"vision": true})),
        "image" => Some(serde_json::json!({"imageOutput": true})),
        "stt" => Some(serde_json::json!({"audioInput": true})),
        "tts" => Some(serde_json::json!({"audioOutput": true})),
        "embedding" => Some(serde_json::json!({"tools": false})),
        _ => None,
    }
}

/// Check if model is a Gemini 4 variant
pub fn is_gemini4_model(model: Option<&str>) -> bool {
    match model {
        Some(m) => m.to_lowercase().contains("gemini-4"),
        None => false,
    }
}

/// Parse OpenAI model capabilities from model id patterns
pub fn parse_openai_model_capabilities(model: Option<&str>) -> Option<Json> {
    let model = match model {
        Some(m) => m,
        None => return None,
    };

    let lower = model.to_lowercase();
    if !lower.starts_with("gpt") {
        return None;
    }

    // GPT-5+ models have reasoning/thinking
    if lower.contains("gpt-5") || lower.contains("gpt-6") {
        Some(serde_json::json!({
            "reasoning": true,
            "thinkingFormat": "openai"
        }))
    } else {
        Some(serde_json::json!({}))
    }
}

/// Register custom model capabilities
pub fn register_custom_model_capabilities(_model_id: &str, _caps: &Json) {
    // TODO: integrate with custom model registry once persisted
}

/// Set catalog source for dynamic capability updates
pub fn set_catalog_source(_source: &Json) {
    // TODO: integrate with models.dev catalog sync
}

/// Get default capabilities baseline
fn default_capabilities() -> Json {
    crate::export("open-sse/providers/capabilities", "DEFAULT_CAPABILITIES")
        .cloned()
        .unwrap_or_else(|| {
            serde_json::json!({
                "vision": false,
                "pdf": false,
                "audioInput": false,
                "videoInput": false,
                "imageOutput": false,
                "audioOutput": false,
                "search": false,
                "tools": true,
                "reasoning": false,
                "thinkingFormat": null,
                "thinkingCanDisable": true,
                "thinkingRange": null,
                "thinkingEffortSupported": false,
                "contextWindow": 200000,
                "maxOutput": 64000
            })
        })
}

/// Merge capability overrides over defaults
fn merge_over_defaults(overrides: &Json) -> Json {
    let mut result = default_capabilities();
    if let (serde_json::Value::Object(ref mut result_obj), serde_json::Value::Object(overrides_obj)) =
        (&mut result, overrides)
    {
        result_obj.extend(overrides_obj.clone());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_capabilities() {
        let caps = default_capabilities();
        assert_eq!(caps.get("vision").and_then(Json::as_bool), Some(false));
        assert_eq!(caps.get("tools").and_then(Json::as_bool), Some(true));
        assert_eq!(caps.get("contextWindow").and_then(Json::as_u64), Some(200000));
    }

    #[test]
    fn test_capabilities_from_service_kind() {
        let img_caps = capabilities_from_service_kind(Some("image"));
        assert_eq!(img_caps.and_then(|c| c.get("imageOutput").and_then(Json::as_bool)), Some(true));

        let vision_caps = capabilities_from_service_kind(Some("imageToText"));
        assert_eq!(vision_caps.and_then(|c| c.get("vision").and_then(Json::as_bool)), Some(true));
    }

    #[test]
    fn test_is_gemini4_model() {
        assert!(is_gemini4_model(Some("gemini-4-pro")));
        assert!(!is_gemini4_model(Some("gemini-3.5-sonnet")));
    }
}
