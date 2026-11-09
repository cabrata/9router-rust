use nr_core::Json;
use std::collections::HashMap;
use std::sync::LazyLock;

/// Get valid thinking levels for a model.
pub fn get_thinking_levels(provider: Option<&str>, model: Option<&str>) -> Option<Vec<String>> {
    let provider = provider.unwrap_or("");
    let model = model.unwrap_or("");

    // TODO(dedupe): use capabilities::get_capabilities_for_model once ported
    // Placeholder: return None if model has no reasoning
    if !model.contains("reason") && !model.contains("think") {
        return None;
    }

    // TODO(dedupe): use kiro_constants::resolve_kiro_effort_path once ported
    if provider == "kiro" {
        // Kiro models have specific thinking support
    }

    // TODO(dedupe): use pricing::match_pattern once ported
    // Placeholder: return base levels for now
    Some(vec![
        "none".to_string(),
        "low".to_string(),
        "medium".to_string(),
        "high".to_string(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_thinking_levels() {
        // Test that reasoning models return some levels
        let levels = get_thinking_levels(Some("openai"), Some("gpt-4-reasoning"));
        assert!(levels.is_some());

        // Test that non-reasoning models return None
        let levels = get_thinking_levels(Some("openai"), Some("gpt-4"));
        assert!(levels.is_none());
    }
}
