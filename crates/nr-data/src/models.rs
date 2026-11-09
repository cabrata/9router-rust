use nr_core::Json;
use std::collections::HashMap;
use std::sync::LazyLock;

static DEFAULT_MODEL_INFO: LazyLock<Json> = LazyLock::new(|| {
    serde_json::json!({
        "type": ["chat"],
        "contextWindow": 200000,
    })
});

static MODEL_INFO: LazyLock<HashMap<String, Json>> = LazyLock::new(|| {
    // MODEL_INFO is empty in the JS source
    HashMap::new()
});

/// Resolve model metadata with defaults for contextWindow and type.
pub fn get_model_info(model_id: &str) -> Json {
    let mut result = DEFAULT_MODEL_INFO.clone();
    if let Some(custom) = MODEL_INFO.get(model_id) {
        // Merge custom over defaults
        if let serde_json::Value::Object(mut obj) = result {
            if let serde_json::Value::Object(custom_obj) = custom {
                obj.extend(custom_obj.clone());
            }
            return serde_json::Value::Object(obj);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_model_info_defaults() {
        let info = get_model_info("gpt-4");
        assert_eq!(info.get("contextWindow").and_then(|v| v.as_u64()), Some(200000));
        assert_eq!(info.get("type").and_then(|v| v.as_array()).map(|a| a.len()), Some(1));
    }
}
