//! JSON utilities. Port of open-sse/translator/concerns/json.js.

use nr_core::Json;

/// Safe JSON parse: non-string passthrough, on error return fallback.
pub fn safe_parse_json(s: &str, fallback: Json) -> Json {
    match serde_json::from_str(s) {
        Ok(v) => v,
        Err(_) => fallback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_safe_parse_valid() {
        let result = safe_parse_json(r#"{"x": 1}"#, Json::Null);
        assert_eq!(result.get("x").and_then(|v| v.as_i64()), Some(1));
    }

    #[test]
    fn test_safe_parse_invalid() {
        let result = safe_parse_json("not json", Json::Null);
        assert_eq!(result, Json::Null);
    }
}
