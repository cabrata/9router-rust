//! Pre-fetch remote image URLs into base64. Port of open-sse/translator/concerns/prefetch.js.

use once_cell::sync::Lazy;
use std::collections::HashSet;

use nr_core::Json;

use crate::concerns::image::{fetch_image_as_base64, parse_data_uri, FetchOptions};

// Targets that require inline base64 images (cannot accept remote URLs).
static TARGETS_NEED_BASE64: &[&str] = &[
    "gemini", "gemini-cli", "vertex", "antigravity", "ollama", "kiro", "commandcode",
];

fn is_remote_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// Replace remote image URLs with base64 data when the target needs inline data.
/// No-op when target accepts remote URLs (e.g. openai, claude) or body has none.
/// Returns count of images converted.
pub async fn prefetch_remote_images(
    body: &mut Json,
    source_format: &str,
    target_format: &str,
    options: Option<&FetchOptions>,
) -> usize {
    if !body.is_object() || !TARGETS_NEED_BASE64.contains(&target_format) {
        return 0;
    }
    let default_opts = FetchOptions::default();
    let opts = options.unwrap_or(&default_opts);

    let mut converted = 0;

    match source_format {
        "claude" => {
            if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
                for msg in messages {
                    let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else { continue };
                    for block in content {
                        if block.get("type").and_then(|v| v.as_str()) != Some("image") {
                            continue;
                        }
                        let source = match block.get_mut("source") {
                            Some(s) if s.is_object() => s,
                            _ => continue,
                        };
                        if source.get("type").and_then(|v| v.as_str()) != Some("url") {
                            continue;
                        }
                        let Some(url) = source.get("url").and_then(|u| u.as_str()).map(|s| s.to_string()) else { continue };
                        if !is_remote_url(&url) || parse_data_uri(&url).is_some() {
                            continue;
                        }
                        if let Some(fetched) = fetch_image_as_base64(&url, opts).await {
                            if let Some(data) = fetched.url.split(',').nth(1) {
                                *source = serde_json::json!({
                                    "type": "base64",
                                    "media_type": fetched.mime_type,
                                    "data": data
                                });
                                converted += 1;
                            }
                        }
                    }
                }
            }
        }
        "gemini" | "gemini-cli" | "vertex" => {
            if let Some(contents) = body.get_mut("contents").and_then(|c| c.as_array_mut()) {
                converted += prefetch_gemini_contents(contents, opts).await;
            }
        }
        "antigravity" => {
            if let Some(contents) = body
                .get_mut("request")
                .and_then(|r| r.get_mut("contents"))
                .and_then(|c| c.as_array_mut())
            {
                converted += prefetch_gemini_contents(contents, opts).await;
            }
        }
        // "openai" | "ollama" | "kiro" | "cursor" | "commandcode" | default:
        _ => {
            if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
                for msg in messages {
                    let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else { continue };
                    for block in content {
                        if block.get("type").and_then(|v| v.as_str()) != Some("image_url") {
                            continue;
                        }
                        let image_url_val = match block.get_mut("image_url") {
                            Some(v) => v,
                            None => continue,
                        };
                        let url_opt = if let Some(s) = image_url_val.as_str() {
                            Some(s.to_string())
                        } else if let Some(s) = image_url_val.get("url").and_then(|u| u.as_str()) {
                            Some(s.to_string())
                        } else {
                            None
                        };
                        let Some(url) = url_opt else { continue };
                        if !is_remote_url(&url) || parse_data_uri(&url).is_some() {
                            continue;
                        }
                        if let Some(fetched) = fetch_image_as_base64(&url, opts).await {
                            if image_url_val.is_string() {
                                *image_url_val = serde_json::json!(fetched.url);
                            } else if let Some(obj) = image_url_val.as_object_mut() {
                                obj.insert("url".to_string(), serde_json::json!(fetched.url));
                            }
                            converted += 1;
                        }
                    }
                }
            }
        }
    }

    converted
}

async fn prefetch_gemini_contents(contents: &mut [Json], opts: &FetchOptions) -> usize {
    let mut converted = 0;
    for c in contents {
        let Some(parts) = c.get_mut("parts").and_then(|p| p.as_array_mut()) else { continue };
        for part in parts {
            let uri_opt = part.get("fileData").and_then(|f| f.get("fileUri")).and_then(|u| u.as_str()).map(|s| s.to_string());
            let Some(uri) = uri_opt else { continue };
            if !is_remote_url(&uri) || parse_data_uri(&uri).is_some() {
                continue;
            }
            if let Some(fetched) = fetch_image_as_base64(&uri, opts).await {
                if let Some(data) = fetched.url.split(',').nth(1) {
                    if let Some(obj) = part.as_object_mut() {
                        obj.remove("fileData");
                        obj.insert(
                            "inlineData".to_string(),
                            serde_json::json!({
                                "mimeType": fetched.mime_type,
                                "data": data
                            }),
                        );
                        converted += 1;
                    }
                }
            }
        }
    }
    converted
}

/// Helper to check if a target format requires base64 images.
pub fn target_needs_base64(target_format: &str) -> bool {
    static SET: Lazy<HashSet<&'static str>> = Lazy::new(|| TARGETS_NEED_BASE64.iter().copied().collect());
    SET.contains(target_format)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_prefetch_remote_images_noop_when_not_needed() {
        let mut body = serde_json::json!({
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hello"}]}
            ]
        });
        let count = prefetch_remote_images(&mut body, "openai", "openai", None).await;
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_target_needs_base64() {
        assert!(target_needs_base64("gemini"));
        assert!(target_needs_base64("antigravity"));
        assert!(!target_needs_base64("openai"));
        assert!(!target_needs_base64("claude"));
    }

    #[tokio::test]
    async fn test_prefetch_skips_inline_data_uri() {
        let mut body = serde_json::json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,abc"}}
                ]}
            ]
        });
        let count = prefetch_remote_images(&mut body, "openai", "gemini", None).await;
        assert_eq!(count, 0);
    }
}
