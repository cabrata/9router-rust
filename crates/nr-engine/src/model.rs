//! Model string parsing and provider resolution. Port of open-sse/services/model.js
//! (`parseModel`, `resolveProviderAlias`, `getModelInfoCore`, `inferProviderFromModelName`).

use nr_core::Json;
use nr_db::repos::get_model_aliases;
use nr_db::Db;

/// Alias → provider id, derived from the embedded registry single-source
/// (id → id, alias → id, aliases[] → id). Media-only providers without a
/// registry transport entry keep explicit aliases.
fn alias_to_provider_id() -> &'static std::collections::HashMap<String, String> {
    use std::sync::LazyLock;
    static MAP: LazyLock<std::collections::HashMap<String, String>> = LazyLock::new(|| {
        let mut map = std::collections::HashMap::new();
        // Media-only aliases (MEDIA_ONLY_ALIASES)
        for (a, id) in [
            ("el", "elevenlabs"),
            ("jina", "jina-ai"),
            ("jina-ai", "jina-ai"),
            ("polly", "aws-polly"),
            ("aws-polly", "aws-polly"),
        ] {
            map.insert(a.to_string(), id.to_string());
        }
        for entry in nr_data::registry() {
            let Some(id) = entry.get("id").and_then(Json::as_str) else { continue };
            map.insert(id.to_string(), id.to_string());
            if let Some(alias) = entry.get("alias").and_then(Json::as_str) {
                map.insert(alias.to_string(), id.to_string());
            }
            if let Some(aliases) = entry.get("aliases").and_then(Json::as_array) {
                for a in aliases.iter().filter_map(Json::as_str) {
                    map.insert(a.to_string(), id.to_string());
                }
            }
        }
        map
    });
    &MAP
}

/// Resolve a provider alias to its canonical provider id (JS `resolveProviderAlias`).
pub fn resolve_provider_alias(alias_or_id: &str) -> String {
    alias_to_provider_id()
        .get(alias_or_id)
        .cloned()
        .unwrap_or_else(|| alias_or_id.to_string())
}

/// Parsed `provider/model` string.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedModel {
    pub provider: Option<String>,
    pub model: Option<String>,
    /// True when the string had no `/` (a model alias, not provider/model).
    pub is_alias: bool,
}

/// Parse `alias/model`, `provider/model`, or a bare alias (JS `parseModel`).
pub fn parse_model(model_str: &str) -> ParsedModel {
    if model_str.is_empty() {
        return ParsedModel { provider: None, model: None, is_alias: false };
    }
    if let Some(slash) = model_str.find('/') {
        let provider_or_alias = &model_str[..slash];
        let model = &model_str[slash + 1..];
        return ParsedModel {
            provider: Some(resolve_provider_alias(provider_or_alias)),
            model: Some(model.to_string()),
            is_alias: false,
        };
    }
    ParsedModel { provider: None, model: Some(model_str.to_string()), is_alias: true }
}

/// Builtin model aliases (BUILTIN_MODEL_ALIASES).
const BUILTIN_MODEL_ALIASES: &[(&str, &str)] = &[("grok-build", "gcli/grok-build")];

/// Config-driven prefix → provider inference (first match wins, fallback "openai").
/// Port of MODEL_PREFIX_PROVIDERS / inferProviderFromModelName.
pub fn infer_provider_from_model_name(model_name: &str) -> &'static str {
    let m = model_name.to_lowercase();
    let rules: &[(&str, &str)] = &[
        // Codex CLI sends this bare virtual model for auto-review — keep on OAuth Codex (#1398).
        ("^codex-auto-review$", "codex"),
        // Codex-only GPT model slugs (see #4405).
        ("^gpt-[56]\\.", "codex"),
        ("^gpt-6-", "codex"),
        ("^gpt-daybreak-", "codex"),
        ("^gpt-reserve", "codex"),
        ("^claude-", "anthropic"),
        // Bedrock inference profile IDs (us.anthropic.*, global.meta.*, …).
        ("^(us|eu|ap|global)\\.(anthropic|meta|amazon|mistral|xai)\\.", "bedrock"),
        ("^gemini-", "gemini"),
        ("^gpt-", "openai"),
        ("^o[134]", "openai"),
        ("^deepseek-", "openrouter"),
    ];
    for (pat, provider) in rules {
        if regex::Regex::new(pat).map(|re| re.is_match(&m)).unwrap_or(false) {
            return provider;
        }
    }
    "openai"
}

/// Resolve a model alias against the `aliases` map (JS `resolveModelAliasFromMap`).
/// Value is either `"provider/model"` or `{ provider, model }`.
fn resolve_model_alias_from_map(alias: &str, aliases: &Json) -> Option<(String, String)> {
    let resolved = aliases.get(alias)?;
    if let Some(s) = resolved.as_str() {
        let slash = s.find('/')?;
        return Some((
            resolve_provider_alias(&s[..slash]),
            s[slash + 1..].to_string(),
        ));
    }
    if let Some(obj) = resolved.as_object() {
        if let (Some(p), Some(m)) = (obj.get("provider").and_then(Json::as_str), obj.get("model").and_then(Json::as_str)) {
            return Some((resolve_provider_alias(p), m.to_string()));
        }
    }
    None
}

/// Full model info: provider + model, resolving aliases via the DB map and
/// falling back to prefix inference (JS `getModelInfoCore`).
pub fn get_model_info(db: &Db, model_str: &str) -> (Option<String>, String) {
    let parsed = parse_model(model_str);
    if !parsed.is_alias {
        return (parsed.provider, parsed.model.unwrap_or_default());
    }
    let alias = parsed.model.clone().unwrap_or_default();

    let aliases = get_model_aliases(db).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(resolved) = resolve_model_alias_from_map(&alias, &aliases) {
        return (Some(resolved.0), resolved.1);
    }
    for (a, target) in BUILTIN_MODEL_ALIASES {
        if *a == alias {
            if let Some(resolved) = resolve_model_alias_from_map(target, &serde_json::json!({})) {
                return (Some(resolved.0), resolved.1);
            }
            // target is always provider/model
            if let Some(slash) = target.find('/') {
                return (
                    Some(resolve_provider_alias(&target[..slash])),
                    target[slash + 1..].to_string(),
                );
            }
        }
    }
    (Some(infer_provider_from_model_name(&alias).to_string()), alias)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_provider_model() {
        let p = parse_model("openai/gpt-4o");
        assert_eq!(p.provider.as_deref(), Some("openai"));
        assert_eq!(p.model.as_deref(), Some("gpt-4o"));
        assert!(!p.is_alias);
    }

    #[test]
    fn parse_alias_resolves_provider_alias() {
        let p = parse_model("cu/claude-3-5-sonnet");
        assert_eq!(p.provider.as_deref(), Some("cursor"));
        assert_eq!(p.model.as_deref(), Some("claude-3-5-sonnet"));
    }

    #[test]
    fn parse_bare_model_is_alias() {
        let p = parse_model("gpt-4o");
        assert!(p.is_alias);
        assert_eq!(p.provider, None);
        assert_eq!(p.model.as_deref(), Some("gpt-4o"));
    }

    #[test]
    fn empty_model() {
        let p = parse_model("");
        assert_eq!(p.provider, None);
        assert_eq!(p.model, None);
        assert!(!p.is_alias);
    }

    #[test]
    fn infer_prefixes() {
        assert_eq!(infer_provider_from_model_name("gpt-5.6-terra"), "codex");
        assert_eq!(infer_provider_from_model_name("claude-sonnet-4-5"), "anthropic");
        assert_eq!(infer_provider_from_model_name("gemini-2.0-flash"), "gemini");
        assert_eq!(infer_provider_from_model_name("gpt-4o"), "openai");
        assert_eq!(infer_provider_from_model_name("o3-mini"), "openai");
        assert_eq!(infer_provider_from_model_name("deepseek-chat"), "openrouter");
        assert_eq!(
            infer_provider_from_model_name("us.anthropic.claude-sonnet-4-6"),
            "bedrock"
        );
        assert_eq!(infer_provider_from_model_name("llama-3"), "openai");
    }

    #[test]
    fn resolve_provider_alias_known_and_unknown() {
        assert_eq!(resolve_provider_alias("cu"), "cursor");
        assert_eq!(resolve_provider_alias("openai"), "openai");
        assert_eq!(resolve_provider_alias("totally-unknown"), "totally-unknown");
    }

    #[test]
    fn resolve_model_alias_map_shapes() {
        let aliases = serde_json::json!({
            "fast": "openai/gpt-4o-mini",
            "obj": { "provider": "anthropic", "model": "claude-3-haiku" }
        });
        assert_eq!(
            resolve_model_alias_from_map("fast", &aliases),
            Some(("openai".into(), "gpt-4o-mini".into()))
        );
        assert_eq!(
            resolve_model_alias_from_map("obj", &aliases),
            Some(("anthropic".into(), "claude-3-haiku".into()))
        );
        assert_eq!(resolve_model_alias_from_map("missing", &aliases), None);
    }
}
