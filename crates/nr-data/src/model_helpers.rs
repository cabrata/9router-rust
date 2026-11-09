//! Provider model helper functions
use nr_core::Json;

/// Codex review model suffix
pub const CODEX_REVIEW_SUFFIX: &str = "-review";

/// Format constants for OpenCode and multi-endpoint providers
pub mod formats {
    pub const OPENAI: &str = "openai";
    pub const OPENAI_RESPONSES: &str = "openai-responses";
    pub const CLAUDE: &str = "claude";
}

/// Add "-review" variants for Codex models (llm kind only)
pub fn with_codex_review_models(models: &[Json]) -> Vec<Json> {
    models
        .iter()
        .flat_map(|model| {
            let kind = model
                .get("kind")
                .or_else(|| model.get("type"))
                .and_then(Json::as_str)
                .unwrap_or("llm");

            if kind != "llm" {
                return vec![model.clone()];
            }

            let id = model.get("id").and_then(Json::as_str).unwrap_or("");
            if id.ends_with(CODEX_REVIEW_SUFFIX) {
                return vec![model.clone()];
            }

            let mut review = model.clone();
            if let Json::Object(obj) = &mut review {
                let review_id = format!("{}{}", id, CODEX_REVIEW_SUFFIX);
                obj.insert("id".to_string(), Json::String(review_id));

                if let Some(Json::String(name)) = obj.get("name") {
                    obj.insert(
                        "name".to_string(),
                        Json::String(format!("{} Review", name)),
                    );
                }

                if obj.get("upstreamModelId").is_none() && !id.is_empty() {
                    obj.insert("upstreamModelId".to_string(), Json::String(id.to_string()));
                }

                obj.insert("quotaFamily".to_string(), Json::String("review".to_string()));
            }

            vec![model.clone(), review]
        })
        .collect()
}

/// Check if model ID is a Muse Spark model
pub fn is_muse_spark_model(model_id: Option<&str>) -> bool {
    let Some(id) = model_id else {
        return false;
    };

    let clean = id.replace(|c| c == '(' || c == ')', "").trim().to_string();
    let base = if clean.contains('/') {
        clean.split('/').last().unwrap_or(&clean)
    } else {
        &clean
    };

    regex::Regex::new(r"^muse[-_]?spark(?:$|[-_:.\s])")
        .ok()
        .and_then(|re| if re.is_match(&base.to_lowercase()) { Some(true) } else { None })
        .unwrap_or(false)
}

/// Check if model ID is a DeepSeek model (handles thinking level suffixes)
pub fn is_deepseek_model(model_id: Option<&str>) -> bool {
    let Some(id) = model_id else {
        return false;
    };

    let clean = id
        .trim()
        .chars()
        .take_while(|&c| c != '(')
        .collect::<String>()
        .trim()
        .to_string();

    let base = if clean.contains('/') {
        clean.split('/').last().unwrap_or(&clean)
    } else {
        &clean
    };

    base.to_lowercase().starts_with("deepseek-")
}

/// Check if model ID is OpenCode free-tier (ends with "-free")
pub fn is_opencode_free_tier_model(model_id: Option<&str>) -> bool {
    let Some(id) = model_id else {
        return false;
    };

    let clean = id
        .trim()
        .chars()
        .take_while(|&c| c != '(')
        .collect::<String>()
        .trim()
        .to_string();

    let base = if clean.contains('/') {
        clean.split('/').last().unwrap_or(&clean)
    } else {
        &clean
    };

    base.to_lowercase().ends_with("-free")
}

/// Resolve format family for OpenCode models (unknown ids, modelsFetcher fallback)
pub fn opencode_family_formats(model_id: Option<&str>) -> Option<(&'static str, Vec<&'static str>)> {
    let Some(id) = model_id else {
        return None;
    };

    let clean = id
        .trim()
        .chars()
        .take_while(|&c| c != '(')
        .collect::<String>()
        .trim()
        .to_string();

    let base = if clean.contains('/') {
        clean.split('/').last().unwrap_or(&clean)
    } else {
        &clean
    };

    let base_lower = base.to_lowercase();

    // Check patterns in order (specific to generic)
    if regex::Regex::new(r"^(grok|gpt|muse[-_]?spark)")
        .ok()
        .map(|re| re.is_match(&base_lower))
        .unwrap_or(false)
    {
        return Some((formats::OPENAI_RESPONSES, vec![formats::OPENAI_RESPONSES]));
    }

    if base_lower.starts_with("deepseek-v4-") && (base_lower.contains("-pro") || base_lower.contains("-flash")) {
        return Some((
            formats::OPENAI_RESPONSES,
            vec![formats::OPENAI, formats::CLAUDE, formats::OPENAI_RESPONSES],
        ));
    }

    if regex::Regex::new(r"^(minimax|qwen)")
        .ok()
        .map(|re| re.is_match(&base_lower))
        .unwrap_or(false)
    {
        return Some((formats::OPENAI, vec![formats::OPENAI, formats::CLAUDE]));
    }

    if base_lower.starts_with("claude-") {
        return Some((formats::CLAUDE, vec![formats::CLAUDE]));
    }

    None
}
