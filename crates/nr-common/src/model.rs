#[derive(Debug, Clone)]
pub struct ParsedModel {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub is_alias: bool,
    pub provider_alias: Option<String>,
}

/// Resolve provider alias to provider ID
pub fn resolve_provider_alias(alias_or_id: &str) -> String {
    let provider_id = match alias_or_id {
        "el" => "elevenlabs",
        "jina" => "jina-ai",
        "polly" => "aws-polly",
        _ => alias_or_id,
    };
    provider_id.to_string()
}

/// Parse model string: "alias/model" or "provider/model" or just alias
pub fn parse_model(model_str: &str) -> ParsedModel {
    if model_str.is_empty() {
        return ParsedModel {
            provider: None,
            model: None,
            is_alias: false,
            provider_alias: None,
        };
    }

    if let Some(slash_pos) = model_str.find('/') {
        let provider_or_alias = &model_str[..slash_pos];
        let model = &model_str[slash_pos + 1..];
        let provider = resolve_provider_alias(provider_or_alias);

        ParsedModel {
            provider: Some(provider),
            model: Some(model.to_string()),
            is_alias: false,
            provider_alias: Some(provider_or_alias.to_string()),
        }
    } else {
        ParsedModel {
            provider: None,
            model: Some(model_str.to_string()),
            is_alias: true,
            provider_alias: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub provider: Option<String>,
    pub model: Option<String>,
}

/// Infer provider from model name prefix
fn infer_provider_from_model_name(model_name: &str) -> String {
    let m = model_name.to_lowercase();

    // Config-driven prefix → provider inference (first match wins)
    if m == "codex-auto-review" {
        return "codex".to_string();
    }
    if m.starts_with("gpt-5.") || m.starts_with("gpt-6-") || m.starts_with("gpt-daybreak-") || m.starts_with("gpt-reserve") {
        return "codex".to_string();
    }
    if m.starts_with("claude-") {
        return "anthropic".to_string();
    }
    if m.starts_with("us.") || m.starts_with("eu.") || m.starts_with("ap.") || m.starts_with("global.") {
        if m.contains(".anthropic.") || m.contains(".meta.") || m.contains(".amazon.") || m.contains(".mistral.") || m.contains(".xai.") {
            return "bedrock".to_string();
        }
    }
    if m.starts_with("gemini-") {
        return "gemini".to_string();
    }
    if m.starts_with("gpt-") || m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4") {
        return "openai".to_string();
    }
    if m.starts_with("deepseek-") {
        return "openrouter".to_string();
    }

    "openai".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_model_with_provider() {
        let parsed = parse_model("openai/gpt-4");
        assert_eq!(parsed.provider, Some("openai".to_string()));
        assert_eq!(parsed.model, Some("gpt-4".to_string()));
        assert!(!parsed.is_alias);
    }

    #[test]
    fn test_parse_model_alias() {
        let parsed = parse_model("my-model-alias");
        assert_eq!(parsed.provider, None);
        assert_eq!(parsed.model, Some("my-model-alias".to_string()));
        assert!(parsed.is_alias);
    }

    #[test]
    fn test_resolve_provider_alias() {
        assert_eq!(resolve_provider_alias("el"), "elevenlabs");
        assert_eq!(resolve_provider_alias("openai"), "openai");
    }

    #[test]
    fn test_infer_provider_from_model_name() {
        assert_eq!(infer_provider_from_model_name("gpt-4"), "openai");
        assert_eq!(infer_provider_from_model_name("claude-3"), "anthropic");
        assert_eq!(infer_provider_from_model_name("gemini-pro"), "gemini");
    }
}
