//! TTS models configuration and voice mapping
use nr_core::Json;

/// Get voice options for a TTS model
pub fn get_tts_voices_for_model(provider: &str, model_id: &str) -> Option<Vec<Json>> {
    let cfg = crate::export("open-sse/config/ttsModels", "TTS_MODELS_CONFIG")?
        .get(provider)?;

    let voices = cfg.get("voices")?.as_object()?;
    if let Some(model_voices) = voices.get(model_id) {
        return model_voices.as_array().map(Vec::clone);
    }

    cfg.get("allVoices")
        .and_then(Json::as_array)
        .map(Vec::clone)
}

/// Build TTS provider model entries for PROVIDER_MODELS backward compatibility
pub fn build_tts_provider_models() -> Vec<(String, Json)> {
    let mut entries = Vec::new();

    if let Some(cfg_obj) = crate::export("open-sse/config/ttsModels", "TTS_MODELS_CONFIG")
        .and_then(Json::as_object)
    {
        for (provider, cfg) in cfg_obj.iter() {
            if let Some(models) = cfg.get("models") {
                entries.push((format!("{}-tts-models", provider), models.clone()));
            }
            if let Some(all_voices) = cfg.get("allVoices") {
                entries.push((format!("{}-tts-voices", provider), all_voices.clone()));
            }
            if let Some(defaults) = cfg.get("defaults") {
                entries.push((provider.clone(), defaults.clone()));
            }
        }
    }

    // Keep backward compat keys for openai
    if let Some(openai_voices) = crate::export("open-sse/config/ttsModels", "TTS_MODELS_CONFIG")
        .and_then(|cfg| cfg.get("openai"))
        .and_then(|oai| oai.get("allVoices"))
    {
        entries.push(("openai-tts-voices".to_string(), openai_voices.clone()));
    }

    if let Some(openrouter_voices) = crate::export("open-sse/config/ttsModels", "TTS_MODELS_CONFIG")
        .and_then(|cfg| cfg.get("openrouter"))
        .and_then(|oai| oai.get("allVoices"))
    {
        entries.push(("openrouter-tts-voices".to_string(), openrouter_voices.clone()));
    }

    entries
}
