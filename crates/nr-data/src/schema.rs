use nr_core::{Json, JsonMap};
use serde_json::json;

/// Resolve provider entry with PROVIDER_DEFAULTS and ENDPOINT_DEFAULTS merged in.
/// Deep-merges transport config over defaults (headers, auth, quirks, retry).
pub fn resolve_provider(entry: Option<&Json>) -> Json {
    let transport = entry
        .and_then(|e| e.get("transport"))
        .and_then(|t| t.as_object())
        .cloned()
        .unwrap_or_default();

    let mut result = json!({
        "baseUrl": "",
        "format": "openai",
        "headers": {},
        "auth": {
            "header": "Authorization",
            "scheme": "bearer",
            "source": ["accessToken", "apiKey"]
        },
        "forceStream": false,
        "urlSuffix": "",
        "quirks": {},
        "passthroughModels": false,
        "retry": {
            "maxAttempts": 3,
            "retryAfterMs": 1000,
            "maxRetryAfterMs": 60000
        },
        "timeoutMs": 60000,
        "executor": "default"
    });

    // Merge transport fields (shallow merge for most, deep for nested objects)
    if let Json::Object(ref mut obj) = result {
        for (k, v) in transport.iter() {
            match k.as_str() {
                "headers" | "auth" | "quirks" | "retry" => {
                    // Deep merge these
                    if let (Some(Json::Object(existing)), Json::Object(new)) = (obj.get(k), v) {
                        let mut merged = existing.clone();
                        merged.extend(new.clone());
                        obj.insert(k.clone(), Json::Object(merged));
                    } else {
                        obj.insert(k.clone(), v.clone());
                    }
                }
                _ => {
                    obj.insert(k.clone(), v.clone());
                }
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_provider_defaults() {
        let result = resolve_provider(None);
        assert_eq!(result.get("format").and_then(Json::as_str), Some("openai"));
        assert_eq!(result.get("executor").and_then(Json::as_str), Some("default"));
    }

    #[test]
    fn test_resolve_provider_merges_transport() {
        let entry = json!({
            "transport": {
                "baseUrl": "https://api.example.com",
                "format": "claude"
            }
        });
        let result = resolve_provider(Some(&entry));
        assert_eq!(result.get("baseUrl").and_then(Json::as_str), Some("https://api.example.com"));
        assert_eq!(result.get("format").and_then(Json::as_str), Some("claude"));
    }
}
