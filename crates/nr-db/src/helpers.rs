use serde_json::Value;

#[cfg(test)]
use serde_json::json;

/// Parse a JSON string, returning fallback if null or parsing fails
pub fn parse_json(s: Option<String>, fallback: Value) -> Value {
    match s {
        None => fallback,
        Some(s) => {
            if s.is_empty() {
                fallback
            } else {
                serde_json::from_str(&s).unwrap_or(fallback)
            }
        }
    }
}

/// Serialize a value to JSON, handling null as "null"
pub fn stringify_json(value: &Value) -> String {
    value.to_string()
}

/// Sanitize strings to remove NUL bytes (SQLite TEXT cannot contain them).
/// Similar to JS stripNullBytes.
pub fn strip_null_bytes(s: &str) -> String {
    s.chars().filter(|&c| c != '\0').collect()
}

/// Strip NUL bytes from a value parameter before binding
pub fn clean_param(v: String) -> String {
    strip_null_bytes(&v)
}

/// Normalize an allow list: strings only, trimmed, deduplicated case-insensitively
pub fn normalize_allow_list(list: Option<Vec<String>>) -> Vec<String> {
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();

    if let Some(items) = list {
        for item in items {
            let trimmed = item.trim();
            if trimmed.is_empty() {
                continue;
            }
            let lower = trimmed.to_lowercase();
            if seen.insert(lower) {
                result.push(trimmed.to_string());
            }
        }
    }

    result
}

/// Mask an API key for logging: keep first 8 chars and last 4
pub fn mask_api_key(key: Option<&str>) -> Option<String> {
    key.and_then(|k| {
        let trimmed = k.trim();
        if trimmed.is_empty() {
            None
        } else if trimmed.len() <= 12 {
            Some(format!("{}***", trimmed.chars().next().unwrap_or('?')))
        } else {
            Some(format!(
                "{}***{}",
                &trimmed[..8],
                &trimmed[trimmed.len() - 4..]
            ))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_json() {
        let result = parse_json(Some(r#"{"a":1}"#.to_string()), json!({}));
        assert_eq!(result.get("a").and_then(|v| v.as_i64()), Some(1));
    }

    #[test]
    fn test_parse_json_fallback() {
        let result = parse_json(None, json!({"default": true}));
        assert_eq!(result.get("default").and_then(|v| v.as_bool()), Some(true));
    }

    #[test]
    fn test_strip_null_bytes() {
        let result = strip_null_bytes("hello\0world");
        assert_eq!(result, "helloworld");
    }

    #[test]
    fn test_normalize_allow_list() {
        let list = vec!["Model1".to_string(), "model1".to_string(), "  ".to_string()];
        let result = normalize_allow_list(Some(list));
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_mask_api_key() {
        let masked = mask_api_key(Some("12345678901234567890"));
        assert_eq!(masked, Some("12345678***7890".to_string()));
    }
}
