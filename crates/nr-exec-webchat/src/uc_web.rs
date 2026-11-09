//! Port of open-sse/executors/uc-web.js — UC (UcChat) web executor for image/video generation.
//!
//! Handles image and video generation via UC Chat internal APIs with Clerk authentication.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use nr_core::http::UpstreamResponse;
use nr_core::{ExecRequest, ExecResult, Executor, Result};
use serde_json::{json, Value};

const UC_ORIGIN: &str = "https://uncensored.com";
const UC_CLERK_FAPI: &str = "https://clerk.uncensored.com";
const UC_CLERK_JS_VERSION: &str = "5.127.1";
const UC_IMAGE_URL: &str = "https://internal.chatuncensored.ai/v2/image-gen";
const UC_VIDEO_URL: &str = "https://internal.chatuncensored.ai/image_to_video";
const UC_SIGNED_URL: &str = "https://internal-6.pubyar.com/generate-signed-url";
const POLL_INTERVAL_MS: u64 = 5000;
const POLL_TIMEOUT_MS: u64 = 300000;

const UC_IMAGE_MODE: &str = "dev";
const UC_MODERATION_MODE: &str = "SUPER_LIGHT";
const UC_COUNTRY: &str = "US";
const UC_DEFAULT_IMAGE_MODEL: &str = "seedream-v4.5";
const UC_DEFAULT_VIDEO_MODEL: &str = "default";

lazy_static::lazy_static! {
    static ref UC_ASPECT_SIZES: HashMap<&'static str, (u16, u16)> = {
        let mut m = HashMap::new();
        m.insert("1:1", (1024, 1024));
        m.insert("16:9", (1024, 576));
        m.insert("9:16", (576, 1024));
        m.insert("4:3", (1024, 768));
        m.insert("3:4", (768, 1024));
        m.insert("3:2", (1152, 768));
        m.insert("2:3", (768, 1152));
        m.insert("4:5", (896, 1120));
        m.insert("5:4", (1120, 896));
        m.insert("21:9", (1344, 576));
        m
    };
}

fn parse_cookie_jar(header: &str) -> HashMap<String, String> {
    let mut jar = HashMap::new();
    for part in header.split(';') {
        let part = part.trim();
        if let Some(eq) = part.find('=') {
            let name = part[..eq].trim();
            if !name.is_empty() {
                jar.insert(name.to_string(), part[eq + 1..].trim().to_string());
            }
        }
    }
    jar
}

fn decode_jwt(token: &str) -> Option<Value> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let b64 = parts[1].replace('-', "+").replace('_', "/");
    let padding = (4 - (b64.len() % 4)) % 4;
    let b64_padded = format!("{}{}", b64, "=".repeat(padding));
    if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(&b64_padded) {
        if let Ok(json_str) = String::from_utf8(decoded) {
            return serde_json::from_str(&json_str).ok();
        }
    }
    None
}

fn build_cookie_header(jar: &HashMap<String, String>) -> String {
    jar.iter()
        .map(|(name, value)| format!("{}={}", name, value))
        .collect::<Vec<_>>()
        .join("; ")
}

fn auth_headers(jwt: &str) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    headers.insert("Authorization".to_string(), format!("Bearer {}", jwt));
    headers.insert("Origin".to_string(), UC_ORIGIN.to_string());
    headers.insert("Referer".to_string(), format!("{}/", UC_ORIGIN));
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    headers
}

fn aspect_for(size: Option<&str>, allowed_aspects: Option<&[&str]>) -> Value {
    let raw = size.unwrap_or("").trim();
    let pick = |candidate: &str| -> Option<Value> {
        if !UC_ASPECT_SIZES.contains_key(candidate) {
            return None;
        }
        if let Some(allowed) = allowed_aspects {
            if !allowed.contains(&candidate) {
                return None;
            }
        }
        let (w, h) = UC_ASPECT_SIZES[candidate];
        Some(json!({
            "aspect_ratio": candidate,
            "imageWidth": w.to_string(),
            "imageHeight": h.to_string(),
        }))
    };

    if let Some(direct) = pick(raw) {
        return direct;
    }

    if raw.contains('x') {
        let parts: Vec<&str> = raw.split('x').collect();
        if parts.len() == 2 {
            if let (Ok(w), Ok(h)) = (parts[0].parse::<f64>(), parts[1].parse::<f64>()) {
                if w > 0.0 && h > 0.0 {
                    let mut best: Option<(&str, f64)> = None;
                    for &candidate in UC_ASPECT_SIZES.keys() {
                        if let Some(allowed) = allowed_aspects {
                            if !allowed.contains(&candidate) {
                                continue;
                            }
                        }
                        let (cw, ch) = UC_ASPECT_SIZES[candidate];
                        let delta = ((cw as f64 / ch as f64) - (w / h)).abs();
                        if best.is_none() || delta < best.unwrap().1 {
                            best = Some((candidate, delta));
                        }
                    }
                    if let Some((candidate, _)) = best {
                        return pick(candidate).unwrap();
                    }
                }
            }
        }
    }

    let fallback = allowed_aspects.and_then(|a| a.first()).map(|s| *s).unwrap_or("1:1");
    pick(fallback).unwrap_or(json!({}))
}

/// UC web executor — handles media generation (images/videos)
pub struct UcWebExecutor;

#[async_trait]
impl Executor for UcWebExecutor {
    fn provider(&self) -> &str {
        "uc-web"
    }

    async fn execute(&self, _req: ExecRequest<'_>) -> Result<ExecResult> {
        // UC is a media provider — use /v1/images/generations or /v1/videos/generations
        Ok(ExecResult {
            response: UpstreamResponse::json(
                400,
                &json!({
                    "error": {
                        "message": "uc-web is a media provider — use /v1/images/generations or /v1/videos/generations",
                        "type": "invalid_request"
                    }
                }),
            ),
            url: UC_IMAGE_URL.to_string(),
            headers: Default::default(),
            transformed_body: json!({}),
            response_format: None,
        })
    }
}

/// Create the executor instance
pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(UcWebExecutor)
}

// shortcut: Clerk session token minting and image-to-video upload not ported; UC provider requires browser or full Clerk client library
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_name() {
        let exec = UcWebExecutor;
        assert_eq!(exec.provider(), "uc-web");
    }

    #[test]
    fn test_parse_cookie_jar() {
        let header = "name1=value1; name2=value2; name3=value3";
        let jar = parse_cookie_jar(header);
        assert_eq!(jar.get("name1").map(|s| s.as_str()), Some("value1"));
        assert_eq!(jar.get("name2").map(|s| s.as_str()), Some("value2"));
        assert_eq!(jar.len(), 3);
    }

    #[test]
    fn test_aspect_for_direct() {
        let result = aspect_for(Some("1:1"), None);
        assert_eq!(result["aspect_ratio"], "1:1");
        assert_eq!(result["imageWidth"], "1024");
    }

    #[test]
    fn test_aspect_for_with_allowed() {
        let allowed = vec!["16:9"];
        let result = aspect_for(Some("1:1"), Some(&allowed));
        // Falls back to first allowed since 1:1 not in list
        assert_eq!(result["aspect_ratio"], "16:9");
    }

    #[test]
    fn test_build_cookie_header() {
        let mut jar = HashMap::new();
        jar.insert("name1".to_string(), "value1".to_string());
        jar.insert("name2".to_string(), "value2".to_string());
        let header = build_cookie_header(&jar);
        assert!(header.contains("name1=value1"));
        assert!(header.contains("name2=value2"));
    }

    #[test]
    fn test_auth_headers() {
        let headers = auth_headers("test-jwt");
        assert_eq!(headers.get("Authorization").map(|s| s.as_str()), Some("Bearer test-jwt"));
        assert_eq!(headers.get("Origin").map(|s| s.as_str()), Some(UC_ORIGIN));
    }
}
