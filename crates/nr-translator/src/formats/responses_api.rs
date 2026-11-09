//! OpenAI Responses API format helpers. Port of open-sse/translator/formats/responsesApi.js.

use nr_core::Json;
use serde_json::json;

const MAX_RESPONSES_CALL_ID_LEN: usize = 64;

/// Normalize Responses API input to array format.
/// Accepts string or array, returns array of message items.
pub fn normalize_responses_input(input: &Json) -> Option<Vec<Json>> {
    if let Some(s) = input.as_str() {
        let text = if s.trim().is_empty() { "..." } else { s };
        return Some(vec![json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": text}]
        })]);
    }

    if let Some(arr) = input.as_array() {
        if arr.is_empty() {
            return Some(vec![json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "..."}]
            })]);
        }
        return Some(arr.clone());
    }

    None
}

/// Clamp Responses API call IDs to max length
pub fn clamp_responses_call_id(id: &str) -> String {
    if id.is_empty() {
        format!("call_{}_{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(), 0)
    } else if id.len() > MAX_RESPONSES_CALL_ID_LEN {
        id[..MAX_RESPONSES_CALL_ID_LEN].to_string()
    } else {
        id.to_string()
    }
}

/// Coerce Responses API arguments to valid JSON string
pub fn coerce_responses_arguments(value: &Json) -> String {
    match value {
        Json::Null | Json::String("") => "{}".to_string(),
        Json::String(s) => {
            if serde_json::from_str::<serde_json::Value>(s).is_ok() {
                s.clone()
            } else {
                "{}".to_string()
            }
        }
        _ => {
            value.to_string()
        }
    }
}

/// Coerce Responses API output to valid string
pub fn coerce_responses_output(value: &Json) -> String {
    match value {
        Json::String(s) => s.clone(),
        Json::Null => String::new(),
        Json::Array(arr) => {
            arr.iter()
                .map(|item| {
                    if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                        text.to_string()
                    } else {
                        item.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("")
        }
        _ => value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_normalize_responses_input_string() {
        let input = json!("hello");
        let result = normalize_responses_input(&input);
        assert!(result.is_some());
        let arr = result.unwrap();
        assert_eq!(arr.len(), 1);
    }

    #[test]
    fn test_normalize_responses_input_empty_array() {
        let input = json!([]);
        let result = normalize_responses_input(&input);
        assert!(result.is_some());
        let arr = result.unwrap();
        assert_eq!(arr.len(), 1);
    }

    #[test]
    fn test_clamp_responses_call_id() {
        let short_id = "call_123";
        assert_eq!(clamp_responses_call_id(short_id), "call_123");

        let long_id = &"x".repeat(100);
        let clamped = clamp_responses_call_id(long_id);
        assert!(clamped.len() <= MAX_RESPONSES_CALL_ID_LEN);
    }

    #[test]
    fn test_coerce_responses_arguments() {
        assert_eq!(coerce_responses_arguments(&Json::Null), "{}");
        assert_eq!(coerce_responses_arguments(&json!("{}")).as_str(), "{}");
    }
}
