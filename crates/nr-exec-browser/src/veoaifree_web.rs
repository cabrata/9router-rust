//! Port of open-sse/executors/veoaifree-web.js
//! veoaifree.com free media generator (WordPress AJAX API, no auth needed)
//!
//! Video: full-video-generate → final-video-results polling → mp4 artifact download
//! Image: banan-image-generator returning URLs or raw base64 parts
//! Rate limited (~6 requests/hour per IP)

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use nr_core::http::{HeaderList, UpstreamResponse};
use nr_core::{ExecRequest, ExecResult, Executor, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

const BASE_URL: &str = "https://veoaifree.com";
const AJAX_URL: &str = "https://veoaifree.com/wp-admin/admin-ajax.php";
const POLL_INTERVAL: Duration = Duration::from_millis(20000);
const MAX_POLLS: u32 = 30; // 10 minutes total
const FETCH_TIMEOUT: Duration = Duration::from_millis(30000);
const ARTIFACT_MAX_BYTES: usize = 100 * 1024 * 1024;
const ARTIFACT_MAX_REDIRECTS: u32 = 3;
const ARTIFACT_READY_MAX_ATTEMPTS: u32 = 12;
const ARTIFACT_READY_RETRY_INTERVAL: Duration = Duration::from_millis(5000);

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneratedMedia {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub b64: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
}

/// VeoAiFree executor for media generation
pub struct VeoAiFreeWebExecutor {
    pub provider: String,
}

impl VeoAiFreeWebExecutor {
    pub fn new() -> Self {
        Self {
            provider: "veoaifree-web".to_string(),
        }
    }
}

impl Default for VeoAiFreeWebExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Executor for VeoAiFreeWebExecutor {
    fn provider(&self) -> &str {
        "veoaifree-web"
    }

    fn no_auth(&self) -> bool {
        true
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        // veoaifree-web is a media provider, not a chat provider
        // It should only be accessed via /v1/images/generations or /v1/videos/generations
        let error_body = json!({
            "error": {
                "message": "veoaifree-web is a media provider — use /v1/images/generations or /v1/videos/generations",
                "type": "invalid_request"
            }
        });

        Ok(ExecResult {
            response: UpstreamResponse::json(400, &error_body),
            url: AJAX_URL.to_string(),
            headers: HeaderList::new(),
            transformed_body: req.body,
            response_format: None,
        })
    }
}

/// Create executor for the canonical provider id
pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(VeoAiFreeWebExecutor::new())
}

pub fn is_private_address(addr: &str) -> bool {
    let lower = addr.to_lowercase();
    let trimmed = lower.trim_start_matches('[').trim_end_matches(']');
    let mapped = if trimmed.starts_with("::ffff:") {
        &trimmed[7..]
    } else {
        trimmed
    };

    if mapped.contains('.') {
        let parts: Vec<&str> = mapped.split('.').collect();
        if parts.len() >= 2 {
            if let (Ok(o1), Ok(o2)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                return o1 == 0
                    || o1 == 10
                    || o1 == 127
                    || o1 >= 224
                    || (o1 == 172 && (16..=31).contains(&o2))
                    || (o1 == 192 && o2 == 168)
                    || (o1 == 169 && o2 == 254);
            }
        }
    }

    // IPv6 checks
    mapped == "::1"
        || mapped.starts_with("fc")
        || mapped.starts_with("fd")
        || mapped.starts_with("fe80")
        || mapped.starts_with("fe9")
        || mapped.starts_with("fea")
        || mapped.starts_with("feb")
}

pub async fn assert_public_host(hostname: &str) -> nr_core::Result<()> {
    let bare = hostname.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        if is_private_address(&ip.to_string()) {
            return Err(nr_core::Error::other("artifact host is a blocked address"));
        }
        return Ok(());
    }

    // Resolve via tokio::net::lookup_host
    let addrs = tokio::net::lookup_host(format!("{bare}:443"))
        .await
        .map_err(|_| nr_core::Error::other("artifact host could not be resolved"))?;

    let mut found = false;
    for socket_addr in addrs {
        found = true;
        let ip_str = socket_addr.ip().to_string();
        if is_private_address(&ip_str) {
            return Err(nr_core::Error::other("artifact host resolves to a blocked address"));
        }
    }
    if !found {
        return Err(nr_core::Error::other("artifact host could not be resolved"));
    }
    Ok(())
}

struct SessionContext {
    client: reqwest::Client,
    cookie_header: String,
}

impl SessionContext {
    fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();
        Self {
            client,
            cookie_header: String::new(),
        }
    }

    fn merge_set_cookies(&mut self, headers: &reqwest::header::HeaderMap) {
        let set_cookies = headers.get_all(reqwest::header::SET_COOKIE);
        let mut jar: HashMap<String, String> = HashMap::new();
        for part in self.cookie_header.split(';').filter(|p| !p.trim().is_empty()) {
            if let Some((k, v)) = part.split_once('=') {
                jar.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        for val in set_cookies {
            if let Ok(s) = val.to_str() {
                if let Some(pair) = s.split(';').next() {
                    if let Some((k, v)) = pair.split_once('=') {
                        if !v.trim().is_empty() {
                            jar.insert(k.trim().to_string(), v.trim().to_string());
                        }
                    }
                }
            }
        }
        self.cookie_header = jar
            .into_iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ");
    }
}

pub fn map_aspect_ratio(size: Option<&str>, prefix: &str) -> String {
    let value = size.unwrap_or("").to_lowercase();
    if value.contains("9:16") || value.contains("portrait") {
        format!("{prefix}_PORTRAIT")
    } else if value.contains("1:1") || value.contains("square") {
        format!("{prefix}_SQUARE")
    } else {
        format!("{prefix}_LANDSCAPE")
    }
}

pub async fn fetch_nonce(ctx: &mut SessionContext) -> nr_core::Result<String> {
    let mut req = ctx.client.get(BASE_URL).header("User-Agent", USER_AGENT);
    if !ctx.cookie_header.is_empty() {
        req = req.header("Cookie", &ctx.cookie_header);
    }
    let res = req.send().await.map_err(|e| nr_core::Error::other(format!("fetch nonce failed: {e}")))?;
    ctx.merge_set_cookies(res.headers());
    let html = res.text().await.map_err(|e| nr_core::Error::other(format!("read nonce body failed: {e}")))?;

    let re = regex::Regex::new(r#"nonce":"([a-f0-9]+)""#).map_err(|e| nr_core::Error::other(e.to_string()))?;
    let nonce = re
        .captures(&html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .ok_or_else(|| {
            nr_core::Error::other("failed to extract CSRF nonce from veoaifree.com (site layout changed or rate limited)")
        })?;
    Ok(nonce)
}

pub async fn post_ajax(
    ctx: &mut SessionContext,
    nonce: &str,
    params: &[(&str, &str)],
) -> nr_core::Result<String> {
    let mut form = HashMap::new();
    form.insert("action", "veo_video_generator");
    form.insert("nonce", nonce);
    for (k, v) in params {
        form.insert(*k, *v);
    }

    let mut req = ctx
        .client
        .post(AJAX_URL)
        .header("User-Agent", USER_AGENT)
        .header("Origin", BASE_URL)
        .header("Referer", format!("{BASE_URL}/"))
        .form(&form);

    if !ctx.cookie_header.is_empty() {
        req = req.header("Cookie", &ctx.cookie_header);
    }

    let res = req.send().await.map_err(|e| nr_core::Error::other(format!("AJAX request failed: {e}")))?;
    ctx.merge_set_cookies(res.headers());
    let text = res.text().await.map_err(|e| nr_core::Error::other(format!("read AJAX body failed: {e}")))?;
    Ok(text)
}

pub async fn download_artifact(ctx: &mut SessionContext, artifact_url: &str) -> nr_core::Result<Vec<u8>> {
    let mut current_url = artifact_url.to_string();
    for _ in 0..=ARTIFACT_MAX_REDIRECTS {
        let parsed = url::Url::parse(&current_url).map_err(|e| nr_core::Error::other(e.to_string()))?;
        if parsed.scheme() != "https" {
            return Err(nr_core::Error::other("artifact URL must use HTTPS"));
        }
        let host = parsed.host_str().ok_or_else(|| nr_core::Error::other("missing host in artifact URL"))?;
        assert_public_host(host).await?;

        let mut req = ctx
            .client
            .get(&current_url)
            .header("User-Agent", USER_AGENT)
            .header("Referer", format!("{BASE_URL}/"));
        if !ctx.cookie_header.is_empty() {
            req = req.header("Cookie", &ctx.cookie_header);
        }

        let res = req.send().await.map_err(|e| nr_core::Error::other(format!("artifact download failed: {e}")))?;
        ctx.merge_set_cookies(res.headers());

        let status = res.status().as_u16();
        if (300..400).contains(&status) {
            let loc = res
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| nr_core::Error::other("artifact redirect is missing a destination"))?;
            let next = parsed.join(loc).map_err(|e| nr_core::Error::other(e.to_string()))?;
            current_url = next.to_string();
            continue;
        }

        if matches!(status, 202 | 404 | 409 | 425) {
            return Err(nr_core::Error::other("artifact not available yet"));
        }

        if !res.status().is_success() {
            return Err(nr_core::Error::other(format!("artifact download failed (HTTP {status})")));
        }

        let content_type = res
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_lowercase();

        if content_type != "video/mp4" && content_type != "application/octet-stream" {
            return Err(nr_core::Error::other(format!("artifact returned unsupported content type ({content_type})")));
        }

        let bytes = res.bytes().await.map_err(|e| nr_core::Error::other(format!("read artifact failed: {e}")))?;
        if bytes.len() > ARTIFACT_MAX_BYTES {
            return Err(nr_core::Error::other("artifact exceeds 100MB limit"));
        }
        if bytes.len() < 12 || &bytes[4..8] != b"ftyp" {
            return Err(nr_core::Error::other("artifact is not a valid MP4 file"));
        }
        return Ok(bytes.to_vec());
    }
    Err(nr_core::Error::other("artifact exceeded the redirect limit"))
}

pub async fn download_artifact_with_retry(ctx: &mut SessionContext, artifact_url: &str) -> nr_core::Result<Vec<u8>> {
    let mut last_err = nr_core::Error::other("artifact did not become available in time");
    for _ in 0..ARTIFACT_READY_MAX_ATTEMPTS {
        match download_artifact(ctx, artifact_url).await {
            Ok(bytes) => return Ok(bytes),
            Err(e) => {
                if !e.to_string().contains("not available yet") {
                    return Err(e);
                }
                last_err = e;
                tokio::time::sleep(ARTIFACT_READY_RETRY_INTERVAL).await;
            }
        }
    }
    Err(last_err)
}

pub async fn generate_media(
    prompt: &str,
    kind: &str,
    size: Option<&str>,
) -> nr_core::Result<Vec<GeneratedMedia>> {
    if kind != "video" && kind != "image" {
        return Err(nr_core::Error::other("veoaifree-web supports image and video generation only"));
    }
    if prompt.trim().is_empty() {
        return Err(nr_core::Error::other("prompt is required"));
    }

    let mut ctx = SessionContext::new();
    let nonce = fetch_nonce(&mut ctx).await?;

    if kind == "image" {
        let aspect = map_aspect_ratio(size, "IMAGE_ASPECT_RATIO");
        let result = post_ajax(
            &mut ctx,
            &nonce,
            &[
                ("promptIMG", prompt),
                ("totalVariationsIMG", "1"),
                ("aspectRatioIMG", &aspect),
                ("actionType", "banan-image-generator"),
            ],
        )
        .await?;
        let trimmed = result.trim();
        if trimmed.is_empty() || trimmed == "0" || trimmed.to_lowercase().contains("error") {
            let snippet = if trimmed.len() > 120 { &trimmed[..120] } else { trimmed };
            return Err(nr_core::Error::other(format!("veoaifree image generation failed: {snippet}")));
        }

        let parts: Vec<GeneratedMedia> = trimmed
            .split(',')
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .map(|part| {
                if part.starts_with("http://") || part.starts_with("https://") {
                    GeneratedMedia {
                        url: Some(part.to_string()),
                        b64: None,
                        mime: None,
                    }
                } else {
                    GeneratedMedia {
                        url: None,
                        b64: Some(part.to_string()),
                        mime: Some("image/png".to_string()),
                    }
                }
            })
            .collect();
        return Ok(parts);
    }

    // Video generation
    let aspect = map_aspect_ratio(size, "VIDEO_ASPECT_RATIO");
    let gen_result = post_ajax(
        &mut ctx,
        &nonce,
        &[
            ("prompt", prompt),
            ("totalVariations", "1"),
            ("aspectRatio", &aspect),
            ("actionType", "full-video-generate"),
        ],
    )
    .await?;
    let scene_data = gen_result.trim();
    if scene_data.is_empty() || scene_data == "0" || scene_data.to_lowercase().contains("error") {
        let snippet = if scene_data.len() > 120 { &scene_data[..120] } else { scene_data };
        return Err(nr_core::Error::other(format!("veoaifree video submit failed: {snippet}")));
    }

    for _poll in 0..MAX_POLLS {
        tokio::time::sleep(POLL_INTERVAL).await;
        let poll_result = post_ajax(
            &mut ctx,
            &nonce,
            &[("sceneData", scene_data), ("actionType", "final-video-results")],
        )
        .await?;
        let trimmed = poll_result.trim();
        if trimmed.is_empty() || trimmed == "0" || trimmed.to_lowercase().contains("error") {
            continue;
        }
        let url = trimmed
            .split([',', '\n'])
            .map(|p| p.trim())
            .find(|p| p.starts_with("http://") || p.starts_with("https://"));

        if let Some(artifact_url) = url {
            let buffer = download_artifact_with_retry(&mut ctx, artifact_url).await?;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&buffer);
            return Ok(vec![GeneratedMedia {
                url: None,
                b64: Some(b64),
                mime: Some("video/mp4".to_string()),
            }]);
        }
    }
    Err(nr_core::Error::other("veoaifree video generation timed out after 10 minutes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_private_address() {
        assert!(is_private_address("127.0.0.1"));
        assert!(is_private_address("10.0.0.1"));
        assert!(is_private_address("192.168.1.1"));
        assert!(is_private_address("172.16.0.1"));
        assert!(is_private_address("::1"));
        assert!(!is_private_address("8.8.8.8"));
        assert!(!is_private_address("1.1.1.1"));
    }

    #[test]
    fn test_map_aspect_ratio() {
        assert_eq!(map_aspect_ratio(Some("9:16"), "VIDEO"), "VIDEO_PORTRAIT");
        assert_eq!(map_aspect_ratio(Some("portrait"), "VIDEO"), "VIDEO_PORTRAIT");
        assert_eq!(map_aspect_ratio(Some("1:1"), "VIDEO"), "VIDEO_SQUARE");
        assert_eq!(map_aspect_ratio(Some("square"), "IMAGE"), "IMAGE_SQUARE");
        assert_eq!(map_aspect_ratio(Some("16:9"), "VIDEO"), "VIDEO_LANDSCAPE");
        assert_eq!(map_aspect_ratio(None, "VIDEO"), "VIDEO_LANDSCAPE");
    }

    #[test]
    fn test_executor_creation() {
        let exec = create("veoaifree-web");
        assert_eq!(exec.provider(), "veoaifree-web");
        assert!(exec.no_auth());
    }
}
