use nr_core::{Json, JsonMap};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

/// PROVIDER_CAPABILITIES[provider][model] overrides from the embedded dump.
static PROVIDER_CAPABILITIES: LazyLock<Json> = LazyLock::new(|| {
    crate::export("open-sse/providers/capabilities", "PROVIDER_CAPABILITIES")
        .cloned()
        .unwrap_or(Json::Object(JsonMap::new()))
});

/// MODEL_CAPABILITIES[model] canonical overrides.
static MODEL_CAPABILITIES: LazyLock<Json> = LazyLock::new(|| {
    crate::export("open-sse/providers/capabilities", "MODEL_CAPABILITIES")
        .cloned()
        .unwrap_or(Json::Object(JsonMap::new()))
});

/// PATTERN_CAPABILITIES ordered glob fallback (first match wins).
static PATTERN_CAPABILITIES: LazyLock<Json> = LazyLock::new(|| {
    crate::export("open-sse/providers/capabilities", "PATTERN_CAPABILITIES")
        .cloned()
        .unwrap_or(Json::Array(Vec::new()))
});

/// Runtime registry for custom & OpenAI-compatible model capabilities.
/// Keys are `"<provider>:<model>"` (lowercased) or plain model ids, mirroring
/// capabilities.js getCustomCapsRegistry().
static CUSTOM_CAPS: LazyLock<Mutex<HashMap<String, Json>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Catalog reader installed by the server at startup (models.dev sync overlay).
/// Held behind a Mutex because capabilities.js keeps it on globalThis so every
/// reader sees the same install; a OnceLock would freeze a null install.
static CATALOG_SOURCE: LazyLock<Mutex<Option<Json>>> = LazyLock::new(|| Mutex::new(None));

const MODALITY_KEYS: &[&str] = &["vision", "pdf", "audioInput", "videoInput"];

/// Get capabilities for a model from the multi-layer fallback.
pub fn get_capabilities_for_model(provider: Option<&str>, model: Option<&str>) -> Json {
    let model = match model {
        Some(m) => m,
        None => return default_capabilities(),
    };

    let base_model = model.split('/').next_back().unwrap_or(model).to_string();
    let effective_provider = provider
        .map(|p| p.to_string())
        .or_else(|| {
            model
                .contains('/')
                .then(|| model.split('/').next().unwrap_or(model).to_string())
        })
        .unwrap_or_default();

    // CommandCode wire is /alpha/generate for every model. Family patterns
    // (deepseek-v4 -> thinkingFormat:deepseek, vision:false) must not win here.
    if effective_provider == "commandcode" || effective_provider == "cmc" {
        let provider_caps = PROVIDER_CAPABILITIES
            .get("commandcode")
            .and_then(|p| p.as_object());
        if let Some(caps) = provider_caps.and_then(|m| m.get(model).or_else(|| m.get(&base_model))) {
            return merge_over_defaults(caps);
        }
        let mut result = default_capabilities();
        if let Json::Object(ref mut obj) = result {
            obj.insert("reasoning".into(), Json::Bool(true));
            obj.insert("thinkingFormat".into(), Json::String("commandcode".into()));
            obj.insert("thinkingEffortSupported".into(), Json::Bool(true));
            obj.insert(
                "vision".into(),
                Json::Bool(!is_commandcode_text_only(model)),
            );
            obj.insert("contextWindow".into(), Json::Number(1_000_000.into()));
            obj.insert("maxOutput".into(), Json::Number(384_000.into()));
        }
        return result;
    }

    // 1. Provider-specific override (full id, then vendor-stripped base id)
    if !effective_provider.is_empty() {
        if let Some(provider_caps) = PROVIDER_CAPABILITIES
            .get(effective_provider.as_str())
            .and_then(|p| p.as_object())
        {
            if let Some(caps) = provider_caps
                .get(model)
                .or_else(|| provider_caps.get(&base_model))
            {
                return merge_over_defaults(caps);
            }
        }
    }

    // 2. Canonical exact, then catalog overlay so provider-scoped models.dev
    //    deltas still apply.
    if let Some(caps) = MODEL_CAPABILITIES.get(&base_model).or_else(|| MODEL_CAPABILITIES.get(model)) {
        return refine(caps, Some(&effective_provider), model);
    }

    // 3. Custom & OpenAI-compatible registry (installed by executors).
    if let Some(caps) = get_custom_model_capabilities(
        if effective_provider.is_empty() { None } else { Some(&effective_provider) },
        Some(model),
    ) {
        return merge_over_defaults(&caps);
    }

    // 4. Pattern match (first match wins), refined by catalog + name heuristic
    if let Some(pattern_caps) = PATTERN_CAPABILITIES.as_array().and_then(|entries| {
        entries.iter().find_map(|entry| {
            let pattern = entry.get("pattern").and_then(Json::as_str)?;
            let caps = entry.get("caps")?;
            if crate::pricing::match_pattern(pattern, Some(&base_model))
                || crate::pricing::match_pattern(pattern, Some(model))
            {
                Some(caps.clone())
            } else {
                None
            }
        })
    }) {
        return refine(&pattern_caps, Some(&effective_provider), model);
    }

    // 5. Floor
    refine(&Json::Null, Some(&effective_provider), model)
}

/// Catalog overlay + vision name heuristic, mirroring capabilities.js refine().
fn refine(caps: &Json, provider: Option<&str>, model: &str) -> Json {
    let mut result = default_capabilities();
    if let Json::Object(obj) = caps {
        if let Json::Object(ref mut out) = result {
            for (key, value) in obj {
                out.insert(key.clone(), value.clone());
            }
        }
    }
    // Catalog overlay: the models.dev sync installs data in place of JS's
    // { getModalities, getLimits } closures, as
    // { "modalities": { provider: { model: {...} } }, "limits": {...} }.
    // Additive only, like the JS refine().
    if let Some(catalog) = CATALOG_SOURCE.lock().ok().and_then(|guard| guard.clone()) {
        if let Some(p) = provider.filter(|p| !p.is_empty()) {
            let node = |section: &str| -> Option<&Json> {
                catalog.get(section).and_then(|s| s.get(p)).and_then(|m| m.get(model))
            };
            if let Some(modalities) = node("modalities") {
                if let Json::Object(ref mut out) = result {
                    for key in MODALITY_KEYS {
                        if modalities.get(*key) == Some(&Json::Bool(true)) {
                            out.insert((*key).into(), Json::Bool(true));
                        }
                    }
                }
            }
            if let Some(limits) = node("limits") {
                if let Json::Object(ref mut out) = result {
                    for key in ["contextWindow", "maxOutput"] {
                        if let Some(v) = limits.get(key).and_then(Json::as_f64).filter(|v| *v > 0.0) {
                            out.insert(key.into(), Json::Number((v as u64).into()));
                        }
                    }
                }
            }
        }
    }
    if !result.get("vision").and_then(Json::as_bool).unwrap_or(false)
        && crate::vision_patterns::looks_like_vision_model(Some(model))
    {
        if let Json::Object(ref mut out) = result {
            out.insert("vision".into(), Json::Bool(true));
        }
    }
    result
}

/// Check if a CommandCode model is text-only (no vision input).
fn is_commandcode_text_only(model: &str) -> bool {
    let key = model.to_lowercase();
    const TEXT_ONLY: &[&str] = &["deepseek", "glm", "qwen", "kimi", "grok", "llama", "minimax"];
    for id in TEXT_ONLY {
        let base = id.split('/').next_back().unwrap_or(id);
        if key == base || key.ends_with(&format!("/{base}")) {
            return true;
        }
    }
    false
}

/// Get capabilities for a custom model added via dashboard.
pub fn get_custom_model_capabilities(provider: Option<&str>, model_id: Option<&str>) -> Option<Json> {
    let model = model_id?;
    let m = model.trim().to_lowercase();
    if m.is_empty() {
        return None;
    }
    let base = m.split('/').next_back().unwrap_or(&m).to_string();
    let p = provider
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .or_else(|| model.split('/').next().map(|s| s.to_lowercase()).filter(|_| model.contains('/')));

    let registry = CUSTOM_CAPS.lock().ok()?;
    if let Some(p) = p {
        if let Some(caps) = registry
            .get(&format!("{p}:{m}"))
            .or_else(|| registry.get(&format!("{p}:{base}")))
        {
            return Some(caps.clone());
        }
    }
    registry.get(&m).or_else(|| registry.get(&base)).cloned()
}

/// Look up capabilities for a custom model by id only (no provider).
pub fn get_custom_model_capabilities_by_id(model_id: Option<&str>) -> Option<Json> {
    get_custom_model_capabilities(None, model_id)
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

/// Parse OpenAI model capabilities from a models.dev-style entry object.
/// Mirrors capabilities.js parseOpenAIModelCapabilities (returns null for
/// non-objects and when no capability field is present).
pub fn parse_openai_model_capabilities(model: Option<&Json>) -> Option<Json> {
    let model = model?;
    let obj = model.as_object()?;
    let num = |keys: &[&str], limit_key: &str| -> Option<f64> {
        keys.iter()
            .find_map(|k| obj.get(*k).and_then(Json::as_f64).filter(|v| v.is_finite()))
            .or_else(|| {
                obj.get("limit")
                    .and_then(|l| l.get(limit_key))
                    .and_then(Json::as_f64)
                    .filter(|v| v.is_finite())
            })
    };
    let flag = |value: Option<&Json>| value.map(nr_core::json::truthy).unwrap_or(false);

    let ctx = num(
        &[
            "context_length", "contextLength", "context_window", "contextWindow",
            "context_window_tokens", "max_context_length", "max_model_len",
            "max_input_tokens",
        ],
        "context",
    );
    let out = num(
        &[
            "max_output_tokens", "maxOutputTokens", "maxOutput",
            "maximum_output_tokens", "max_tokens", "maxTokens",
        ],
        "output",
    );

    let caps_obj = |key: &str| obj.get("capabilities").and_then(|c| c.get(key));
    let vision = ["vision", "attachment", "isVL", "supports_vision"]
        .iter()
        .any(|k| flag(obj.get(*k)))
        || obj
            .get("modalities")
            .and_then(|m| m.get("input"))
            .and_then(Json::as_array)
            .map(|a| a.iter().any(|v| v.as_str() == Some("image")))
            .unwrap_or(false)
        || flag(caps_obj("vision"));

    let reasoning = ["isReasoning", "reasoning", "supports_reasoning"]
        .iter()
        .any(|k| flag(obj.get(*k)))
        || flag(caps_obj("reasoning"));

    let tools = if obj.contains_key("supports_tools")
        || obj.contains_key("tools")
        || caps_obj("tools").is_some()
    {
        obj.get("supports_tools")
            .map(|v| flag(Some(v)))
            .or_else(|| obj.get("tools").map(|v| flag(Some(v))))
            .or_else(|| caps_obj("tools").map(|v| flag(Some(v))))
    } else {
        None
    };

    let mut caps = JsonMap::new();
    if let Some(ctx) = ctx.filter(|v| *v > 0.0) {
        caps.insert("contextWindow".into(), Json::Number((ctx as u64).into()));
    }
    if let Some(out) = out.filter(|v| *v > 0.0) {
        caps.insert("maxOutput".into(), Json::Number((out as u64).into()));
    }
    if vision {
        caps.insert("vision".into(), Json::Bool(true));
    }
    if reasoning {
        caps.insert("reasoning".into(), Json::Bool(true));
    }
    if let Some(tools) = tools {
        caps.insert("tools".into(), Json::Bool(tools));
    }
    if caps.is_empty() {
        None
    } else {
        Some(Json::Object(caps))
    }
}

/// Register custom model capabilities into the runtime registry.
pub fn register_custom_model_capabilities(provider: Option<&str>, model: Option<&str>, caps: &Json) {
    let (Some(model), _) = (model, ()) else { return };
    let parsed = parse_openai_model_capabilities(Some(caps))
        .or_else(|| caps.as_object().map(|_| caps.clone()));
    let Some(parsed) = parsed else { return };

    let m = model.trim().to_lowercase();
    if m.is_empty() {
        return;
    }
    let base = m.split('/').next_back().unwrap_or(&m).to_string();
    let p = provider.map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());

    let Ok(mut registry) = CUSTOM_CAPS.lock() else { return };
    if let Some(p) = p {
        registry.insert(format!("{p}:{m}"), parsed.clone());
        registry.insert(format!("{p}:{base}"), parsed);
    } else {
        registry.insert(m, parsed.clone());
        registry.insert(base, parsed);
    }
}

/// Set the models.dev catalog overlay used by refine() (pass null to clear).
pub fn set_catalog_source(source: Option<&Json>) {
    *CATALOG_SOURCE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
        source.cloned();
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

#[cfg(test)]
mod parity_tests {
    use super::*;
    use crate::Json;

    fn field(caps: &Json, key: &str) -> Json {
        caps.get(key).cloned().unwrap_or(Json::Null)
    }

    #[test]
    fn matches_js_capability_ladder() {
        // Values captured from capabilities.js getCapabilitiesForModel().
        let cases = [
            ("openai", "gpt-5", true, true, "openai", 400_000u64, 128_000u64, true),
            ("anthropic", "claude-opus-4.7", true, true, "claude-adaptive", 1_000_000, 128_000, true),
            ("anthropic", "claude-sonnet-4.6", true, true, "claude-adaptive", 1_000_000, 128_000, true),
            ("google", "gemini-3-pro", true, true, "gemini-level", 1_048_576, 65_535, false),
            ("deepseek", "deepseek-v4-pro", false, true, "deepseek", 1_000_000, 384_000, true),
            ("openai", "gpt-4", false, false, "", 128_000, 64_000, true),
            ("anthropic", "claude-opus-5.5", true, true, "claude-adaptive", 1_000_000, 128_000, false),
            ("openai", "gpt-5.6-sol", true, true, "openai", 1_050_000, 128_000, true),
        ];
        for (provider, model, vision, reasoning, fmt, ctx, max_out, can_disable) in cases {
            let caps = get_capabilities_for_model(Some(provider), Some(model));
            assert_eq!(field(&caps, "vision"), Json::Bool(vision), "{model} vision");
            assert_eq!(field(&caps, "reasoning"), Json::Bool(reasoning), "{model} reasoning");
            assert_eq!(
                caps.get("thinkingFormat").and_then(Json::as_str).unwrap_or(""),
                fmt,
                "{model} thinkingFormat"
            );
            assert_eq!(field(&caps, "contextWindow").as_u64(), Some(ctx), "{model} ctx");
            assert_eq!(field(&caps, "maxOutput").as_u64(), Some(max_out), "{model} maxOut");
            assert_eq!(
                field(&caps, "thinkingCanDisable"),
                Json::Bool(can_disable),
                "{model} canDisable"
            );
            // Result is always complete (defaults merged in).
            assert_eq!(field(&caps, "tools"), Json::Bool(true), "{model} tools");
        }
    }

    #[test]
    fn unknown_model_falls_back_to_defaults() {
        let caps = get_capabilities_for_model(None, Some("some-unknown-model"));
        assert_eq!(field(&caps, "contextWindow").as_u64(), Some(200_000));
        assert_eq!(field(&caps, "maxOutput").as_u64(), Some(64_000));
        assert_eq!(field(&caps, "reasoning"), Json::Bool(false));
    }

    #[test]
    fn none_model_returns_defaults() {
        let caps = get_capabilities_for_model(Some("openai"), None);
        assert_eq!(field(&caps, "contextWindow").as_u64(), Some(200_000));
    }

    #[test]
    fn commandcode_defaults_override_family_patterns() {
        let caps = get_capabilities_for_model(Some("commandcode"), Some("deepseek-v4-pro"));
        assert_eq!(
            caps.get("thinkingFormat").and_then(Json::as_str),
            Some("commandcode")
        );
        assert_eq!(field(&caps, "contextWindow").as_u64(), Some(1_000_000));
        assert_eq!(field(&caps, "maxOutput").as_u64(), Some(384_000));
    }

    #[test]
    fn vendor_prefix_is_stripped_for_canonical_lookup() {
        let caps = get_capabilities_for_model(None, Some("anthropic/claude-opus-4.7"));
        assert_eq!(
            caps.get("thinkingFormat").and_then(Json::as_str),
            Some("claude-adaptive")
        );
    }

    #[test]
    fn pattern_match_applies_for_unlisted_models() {
        // Not in MODEL_CAPABILITIES, matched by the *claude*sonnet* pattern.
        let caps = get_capabilities_for_model(Some("anthropic"), Some("claude-sonnet-9-preview"));
        assert_eq!(field(&caps, "vision"), Json::Bool(true));
        assert_eq!(
            caps.get("thinkingFormat").and_then(Json::as_str),
            Some("claude-budget")
        );
    }

    #[test]
    fn vision_name_heuristic_fills_floor_result() {
        // No table hit, but the model id carries a vision marker.
        let caps = get_capabilities_for_model(Some("unknown-provider"), Some("qwen3-vl-plus"));
        assert_eq!(field(&caps, "vision"), Json::Bool(true));
    }

    #[test]
    fn parse_openai_model_capabilities_reads_models_dev_shape() {
        let entry = serde_json::json!({
            "limit": {"context": 200000, "output": 64000},
            "modalities": {"input": ["text", "image"]},
            "reasoning": true,
            "tools": false
        });
        let caps = parse_openai_model_capabilities(Some(&entry)).expect("caps parsed");
        assert_eq!(field(&caps, "contextWindow").as_u64(), Some(200_000));
        assert_eq!(field(&caps, "maxOutput").as_u64(), Some(64_000));
        assert_eq!(field(&caps, "vision"), Json::Bool(true));
        assert_eq!(field(&caps, "reasoning"), Json::Bool(true));
        assert_eq!(field(&caps, "tools"), Json::Bool(false));
    }

    #[test]
    fn parse_openai_model_capabilities_returns_none_without_fields() {
        assert!(parse_openai_model_capabilities(Some(&serde_json::json!({"id": "x"}))).is_none());
        assert!(parse_openai_model_capabilities(Some(&Json::String("x".into()))).is_none());
        assert!(parse_openai_model_capabilities(None).is_none());
    }

    #[test]
    fn custom_registry_roundtrip() {
        register_custom_model_capabilities(
            Some("openai-compatible-chat-abc"),
            Some("my-vendor/my-model-v1"),
            &serde_json::json!({"contextWindow": 123456, "vision": true}),
        );
        let hit = get_custom_model_capabilities(
            Some("openai-compatible-chat-abc"),
            Some("my-vendor/my-model-v1"),
        )
        .expect("registered caps found");
        assert_eq!(field(&hit, "contextWindow").as_u64(), Some(123_456));
        assert_eq!(field(&hit, "vision"), Json::Bool(true));

        // Vendor-stripped base id resolves too.
        let by_base =
            get_custom_model_capabilities(Some("openai-compatible-chat-abc"), Some("my-model-v1"))
                .expect("base id hit");
        assert_eq!(field(&by_base, "vision"), Json::Bool(true));

        // And it wins over the pattern fallback in the main ladder.
        let caps = get_capabilities_for_model(
            Some("openai-compatible-chat-abc"),
            Some("my-vendor/my-model-v1"),
        );
        assert_eq!(field(&caps, "contextWindow").as_u64(), Some(123_456));
    }

    #[test]
    fn catalog_source_overlays_additively() {
        set_catalog_source(Some(&serde_json::json!({
            "modalities": {"acme": {"acme-1": {"vision": true, "pdf": true}}},
            "limits": {"acme": {"acme-1": {"contextWindow": 5000, "maxOutput": 1234}}}
        })));
        let caps = get_capabilities_for_model(Some("acme"), Some("acme-1"));
        assert_eq!(field(&caps, "vision"), Json::Bool(true));
        assert_eq!(field(&caps, "pdf"), Json::Bool(true));
        assert_eq!(field(&caps, "contextWindow").as_u64(), Some(5_000));
        assert_eq!(field(&caps, "maxOutput").as_u64(), Some(1_234));

        // Clearing restores the floor.
        set_catalog_source(None);
        let caps = get_capabilities_for_model(Some("acme"), Some("acme-1"));
        assert_eq!(field(&caps, "contextWindow").as_u64(), Some(200_000));
    }
}
