//! Model schema normalization and derivation
use nr_core::Json;

/// Derive a display name from a model id using regex patterns
pub fn derive_model_name(model_id: &str) -> String {
    // NAME_PATTERNS from JSON dump
    if let Some(patterns) = crate::export("open-sse/providers/models/namePatterns", "NAME_PATTERNS")
        .and_then(Json::as_array)
    {
        for pattern_entry in patterns {
            if let Some(pair) = pattern_entry.as_array() {
                if pair.len() >= 2 {
                    // Pattern and replacement function are JSON-serialized
                    // Try to match common patterns inline
                    if let Some(pattern_str) = pair[0].as_str() {
                        if try_match_pattern(pattern_str, model_id).is_some() {
                            if let Some(result) = apply_pattern(pattern_str, model_id) {
                                return result;
                            }
                        }
                    }
                }
            }
        }
    }

    // Fallback: return id as-is
    model_id.to_string()
}

/// Try to match a pattern string against a model ID
fn try_match_pattern(pattern: &str, model_id: &str) -> Option<()> {
    // Hard-coded patterns from namePatterns.js
    if pattern.contains("kimi-k") {
        if regex::Regex::new(r"^kimi-k(\d+(?:\.\d+)?)(-thinking)?$")
            .ok()
            .map(|re| re.is_match(model_id))
            .unwrap_or(false)
        {
            return Some(());
        }
    }

    if pattern.contains("glm-") {
        if regex::Regex::new(r"^glm-(\d+(?:\.\d+)?)(v)?$")
            .ok()
            .map(|re| re.is_match(model_id))
            .unwrap_or(false)
        {
            return Some(());
        }
    }

    if pattern.contains("minimax-m") {
        if regex::Regex::new(r"^minimax-m(\d+(?:\.\d+)?)$")
            .ok()
            .map(|re| re.is_match(model_id))
            .unwrap_or(false)
        {
            return Some(());
        }
    }

    if pattern.contains("gpt-") {
        if regex::Regex::new(r"^gpt-(.+)$")
            .ok()
            .map(|re| re.is_match(model_id))
            .unwrap_or(false)
        {
            return Some(());
        }
    }

    if pattern.contains("gemini-") {
        if regex::Regex::new(r"^gemini-(.+)$")
            .ok()
            .map(|re| re.is_match(model_id))
            .unwrap_or(false)
        {
            return Some(());
        }
    }

    if pattern.contains("grok-") {
        if regex::Regex::new(r"^grok-(.+)$")
            .ok()
            .map(|re| re.is_match(model_id))
            .unwrap_or(false)
        {
            return Some(());
        }
    }

    if pattern.contains("deepseek-") {
        if regex::Regex::new(r"^deepseek-(.+)$")
            .ok()
            .map(|re| re.is_match(model_id))
            .unwrap_or(false)
        {
            return Some(());
        }
    }

    if pattern.contains("qwen") {
        if regex::Regex::new(r"^qwen([\d.]+.*)$")
            .ok()
            .map(|re| re.is_match(model_id))
            .unwrap_or(false)
        {
            return Some(());
        }
    }

    None
}

/// Apply pattern transformation to model ID
fn apply_pattern(pattern: &str, model_id: &str) -> Option<String> {
    fn title_case(s: &str) -> String {
        s.split(|c: char| c == '-' || c == '_' || c == ' ')
            .filter(|s| !s.is_empty())
            .map(|w| {
                if w.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                    w.to_string()
                } else {
                    let mut chars = w.chars();
                    match chars.next() {
                        None => String::new(),
                        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    }
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    if pattern.contains("kimi-k") {
        if let Ok(re) = regex::Regex::new(r"^kimi-k(\d+(?:\.\d+)?)(-thinking)?$") {
            if let Some(caps) = re.captures(model_id) {
                let version = &caps[1];
                let thinking = if caps.get(2).is_some() { " Thinking" } else { "" };
                return Some(format!("Kimi K{}{}", version, thinking));
            }
        }
    }

    if pattern.contains("glm-") {
        if let Ok(re) = regex::Regex::new(r"^glm-(\d+(?:\.\d+)?)(v)?$") {
            if let Some(caps) = re.captures(model_id) {
                let version = &caps[1];
                let vision = if caps.get(2).is_some() { "V (Vision)" } else { "" };
                return Some(format!("GLM {}{}", version, vision));
            }
        }
    }

    if pattern.contains("minimax-m") {
        if let Ok(re) = regex::Regex::new(r"^minimax-m(\d+(?:\.\d+)?)$") {
            if let Some(caps) = re.captures(model_id) {
                return Some(format!("MiniMax M{}", &caps[1]));
            }
        }
    }

    if pattern.contains("gpt-") {
        if let Ok(re) = regex::Regex::new(r"^gpt-(.+)$") {
            if let Some(caps) = re.captures(model_id) {
                return Some(format!("GPT {}", title_case(&caps[1])));
            }
        }
    }

    if pattern.contains("gemini-") {
        if let Ok(re) = regex::Regex::new(r"^gemini-(.+)$") {
            if let Some(caps) = re.captures(model_id) {
                return Some(format!("Gemini {}", title_case(&caps[1])));
            }
        }
    }

    if pattern.contains("grok-") {
        if let Ok(re) = regex::Regex::new(r"^grok-(.+)$") {
            if let Some(caps) = re.captures(model_id) {
                return Some(format!("Grok {}", title_case(&caps[1])));
            }
        }
    }

    if pattern.contains("deepseek-") {
        if let Ok(re) = regex::Regex::new(r"^deepseek-(.+)$") {
            if let Some(caps) = re.captures(model_id) {
                return Some(format!("DeepSeek {}", title_case(&caps[1])));
            }
        }
    }

    if pattern.contains("qwen") {
        if let Ok(re) = regex::Regex::new(r"^qwen([\d.]+.*)$") {
            if let Some(caps) = re.captures(model_id) {
                return Some(format!("Qwen {}", title_case(&caps[1])));
            }
        }
    }

    None
}

/// Normalize a model entry: accept string (expand to { id }), or fill name via regex
pub fn normalize_model(raw: &Json) -> Json {
    match raw {
        Json::String(id) => {
            let mut obj = nr_core::JsonMap::new();
            obj.insert("id".to_string(), Json::String(id.clone()));
            obj.insert("name".to_string(), Json::String(derive_model_name(id)));
            Json::Object(obj)
        }
        Json::Object(obj) => {
            if obj.contains_key("name") {
                Json::Object(obj.clone())
            } else {
                let mut result = obj.clone();
                if let Some(Json::String(id)) = obj.get("id") {
                    result.insert("name".to_string(), Json::String(derive_model_name(id)));
                }
                Json::Object(result)
            }
        }
        other => other.clone(),
    }
}
