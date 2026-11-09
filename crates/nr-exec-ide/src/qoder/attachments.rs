//! Port of open-sse/shared/qoder/attachments.js.
//!
//! Native qodercli does NOT stuff image/PDF bytes into agent_chat_generation.
//! It PUTs them to `/algo/api/v2/image/upload` (COSY-signed multipart) and then
//! sends the returned OSS URL. Agents like Claude Code send OpenAI/Claude
//! data-URIs instead, which 9router previously forwarded verbatim — 10MB
//! images become 30MB+ JSON and upstream 413s even though the model window is
//! ~200k tokens.
//!
//! This module uploads inlined images to Qoder's file API (cached by sha256
//! within a request), replaces huge non-image file blocks with a short stub,
//! and caps leftover data-URIs.

use std::collections::HashMap;

use base64::Engine;
use nr_core::http::{proxy_fetch, FetchRequest, ProxyOptions};
use nr_core::{Credentials, Error, Json, Logger, Result};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::constants::{qoder_inference_base, QODER_INLINE_FALLBACK_MAX_BYTES, QODER_MAX_PAYLOAD_BYTES};
use super::cosy::{build_cosy_headers, CosyCreds};

const IMAGE_UPLOAD_PATH_WITH_ALGO: &str = "/algo/api/v2/image/upload";

fn max_image_bytes() -> usize {
    nr_data::export("open-sse/config/mediaConfig", "MAX_IMAGE_BYTES")
        .and_then(Json::as_u64)
        .map(|v| v as usize)
        .unwrap_or(nr_translator::concerns::image::DEFAULT_MAX_IMAGE_BYTES)
}

fn mime_ext(mime: &str) -> &'static str {
    let m = mime.to_lowercase();
    if m.contains("png") {
        "png"
    } else if m.contains("jpeg") || m.contains("jpg") {
        "jpg"
    } else if m.contains("gif") {
        "gif"
    } else if m.contains("webp") {
        "webp"
    } else if m.contains("bmp") {
        "bmp"
    } else if m.contains("pdf") {
        "pdf"
    } else {
        "bin"
    }
}

/// `decodedBytes(b64)` — base64 length × 3/4, whitespace excluded.
pub fn decoded_bytes(b64: &str) -> usize {
    let compact: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
    compact.len() * 3 / 4
}

/// `stubText({name, mime, bytes, reason})`.
pub fn stub_text(name: Option<&str>, mime: Option<&str>, bytes: Option<usize>, reason: &str) -> String {
    let label = name
        .filter(|s| !s.is_empty())
        .or_else(|| mime.filter(|s| !s.is_empty()))
        .unwrap_or("attachment");
    let size = bytes.filter(|b| *b > 0).map(|b| format!(", {b} bytes")).unwrap_or_default();
    format!("[file omitted: {label}{size} — {reason}]")
}

/// `buildMultipartFile(buffer, { fieldName, fileName, mediaType })`.
pub fn build_multipart_file(buffer: &[u8], field_name: &str, file_name: &str, media_type: &str) -> (String, Vec<u8>) {
    let boundary = format!(
        "----9routerQoder{:x}{:x}",
        chrono::Utc::now().timestamp_millis(),
        rand::random::<u32>()
    );
    let name = if file_name.is_empty() { format!("upload.{}", mime_ext(media_type)) } else { file_name.to_string() };
    let head = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"{field_name}\"; filename=\"{name}\"\r\nContent-Type: {}\r\n\r\n",
        if media_type.is_empty() { "application/octet-stream" } else { media_type }
    );
    let mut body = Vec::with_capacity(head.len() + buffer.len() + boundary.len() + 8);
    body.extend_from_slice(head.as_bytes());
    body.extend_from_slice(buffer);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (boundary, body)
}

/// `extractUrlFromUploadResponse(json)`.
pub fn extract_url_from_upload_response(value: &Json) -> Option<String> {
    let result = value.get("result").filter(|v| v.is_object()).unwrap_or(value);
    for key in ["imageUrls", "image_urls"] {
        for holder in [result, value] {
            if let Some(arr) = holder.get(key).and_then(Json::as_array) {
                if let Some(first) = arr.first().and_then(Json::as_str).filter(|s| !s.is_empty()) {
                    return Some(first.to_string());
                }
            }
        }
    }
    for key in [
        "imageUrl",
        "image_url",
        "url",
        "ossUrl",
        "oss_url",
        "originalUrl",
        "originUrl",
        "link",
        "image",
    ] {
        for holder in [result, value] {
            if let Some(s) = holder.get(key).and_then(Json::as_str).filter(|s| !s.is_empty()) {
                return Some(s.to_string());
            }
        }
    }
    if let Some(body) = value.get("body").and_then(Json::as_str) {
        if let Ok(parsed) = serde_json::from_str::<Json>(body) {
            return extract_url_from_upload_response(&parsed);
        }
    }
    None
}

fn is_image_mime(mime: &str) -> bool {
    mime.to_lowercase().starts_with("image/")
}

/// Everything one rewrite pass needs.
pub struct RewriteCtx<'a> {
    pub credentials: &'a Credentials,
    pub log: &'a Logger,
    pub proxy: Option<&'a ProxyOptions>,
    pub base_url: String,
    cache: HashMap<String, String>,
    pub uploaded: usize,
    pub stubbed: usize,
}

impl<'a> RewriteCtx<'a> {
    pub fn new(credentials: &'a Credentials, log: &'a Logger, proxy: Option<&'a ProxyOptions>, region: &str) -> Self {
        Self {
            credentials,
            log,
            proxy,
            base_url: qoder_inference_base(&credentials.clone().into_value(), region),
            cache: HashMap::new(),
            uploaded: 0,
            stubbed: 0,
        }
    }

    /// `defaultUploadImage` — COSY-signed multipart PUT.
    async fn upload_image(&mut self, buffer: &[u8], media_type: &str) -> Result<String> {
        let request_id = uuid::Uuid::new_v4().to_string();
        let url = format!(
            "{}{IMAGE_UPLOAD_PATH_WITH_ALGO}?request_id={request_id}",
            self.base_url.trim_end_matches('/')
        );
        let (boundary, body) =
            build_multipart_file(buffer, "file", &format!("image.{}", mime_ext(media_type)), media_type);

        let user_id = self.credentials.psd_str("userId").unwrap_or("");
        let auth_token = self.credentials.access_token().unwrap_or("");
        let name = self.credentials.str("displayName").unwrap_or("");
        let email = self.credentials.str("email").unwrap_or("");
        let machine_id = self.credentials.psd_str("machineId");
        let cosy = build_cosy_headers(
            &body,
            &url,
            &CosyCreds {
                user_id,
                auth_token,
                name,
                email,
                machine_id,
            },
        )
        .ok_or_else(|| Error::Provider("qoder cosy signing failed for image upload".into()))?;

        let mut headers = nr_core::http::HeaderList::new();
        headers.insert("Accept".into(), "application/json".into());
        headers.insert("Content-Type".into(), format!("multipart/form-data; boundary={boundary}"));
        headers.insert("Content-Length".into(), body.len().to_string());
        headers.insert(
            "AI-CLIENT-TIMESTAMP".into(),
            (chrono::Utc::now().timestamp()).to_string(),
        );
        headers.insert("Accept-Encoding".into(), "identity".into());
        for (k, v) in cosy {
            headers.insert(k, v);
        }

        let mut req = FetchRequest::new(http::Method::PUT, url);
        req.headers = headers;
        req.body = Some(body.into());

        let response = proxy_fetch(req, self.proxy).await?;
        if !response.ok() {
            let status = response.status;
            let text = response.text().await.unwrap_or_default();
            let trimmed: String = text.chars().take(180).collect();
            return Err(Error::Provider(format!(
                "HTTP {status}{}",
                if trimmed.is_empty() { String::new() } else { format!(": {trimmed}") }
            )));
        }
        let parsed = response.json_body().await.unwrap_or(Json::Null);
        extract_url_from_upload_response(&parsed)
            .ok_or_else(|| Error::Provider("upload response missing url".into()))
    }

    /// `uploadImageData` — returns the replacement URL, `Keep` (inline is
    /// small enough), or `Stub`.
    async fn upload_image_data(&mut self, base64: &str, media_type: &str) -> UploadOutcome {
        let compact: String = base64.chars().filter(|c| !c.is_whitespace()).collect();
        if compact.is_empty() {
            return UploadOutcome::Keep;
        }
        let bytes = decoded_bytes(&compact);
        if bytes > max_image_bytes() {
            self.log.warn("QODER", format!("image {bytes} bytes exceeds upload cap, stubbing"));
            return UploadOutcome::Stub { bytes };
        }
        let Ok(buffer) = base64::engine::general_purpose::STANDARD.decode(&compact) else {
            return UploadOutcome::Stub { bytes };
        };
        let digest = format!("{:x}", Sha256::digest(&buffer));
        if let Some(url) = self.cache.get(&digest) {
            return UploadOutcome::Url(url.clone());
        }

        match self.upload_image(&buffer, media_type).await {
            Ok(url) if !url.is_empty() => {
                self.cache.insert(digest, url.clone());
                UploadOutcome::Url(url)
            }
            Ok(_) => UploadOutcome::Stub { bytes },
            Err(err) => {
                self.log.warn(
                    "QODER",
                    format!(
                        "image upload failed ({err}); {}",
                        if bytes <= QODER_INLINE_FALLBACK_MAX_BYTES { "keeping inline" } else { "stubbing" }
                    ),
                );
                if bytes <= QODER_INLINE_FALLBACK_MAX_BYTES {
                    UploadOutcome::Keep
                } else {
                    UploadOutcome::Stub { bytes }
                }
            }
        }
    }
}

enum UploadOutcome {
    Url(String),
    Keep,
    Stub { bytes: usize },
}

fn image_url_block(url: &str) -> Json {
    json!({ "type": "image_url", "image_url": { "url": url } })
}

fn text_block(text: String) -> Json {
    json!({ "type": "text", "text": text })
}

async fn rewrite_block(block: &Json, ctx: &mut RewriteCtx<'_>) -> Option<Json> {
    if !block.is_object() {
        return Some(block.clone());
    }
    let block_type = block.get("type").and_then(Json::as_str).unwrap_or("");

    if block_type == "image_url" {
        let raw = match block.get("image_url") {
            Some(Json::String(s)) => s.as_str(),
            Some(v) => v.get("url").and_then(Json::as_str).unwrap_or(""),
            None => "",
        };
        if raw.is_empty() {
            return None;
        }
        if raw.starts_with("http://") || raw.starts_with("https://") {
            return Some(image_url_block(raw));
        }
        let Some(parsed) = nr_translator::concerns::image::parse_data_uri(raw) else {
            return Some(text_block(stub_text(Some("attachment"), None, None, "unreadable data URI")));
        };
        if !is_image_mime(&parsed.mime_type) {
            return Some(text_block(stub_text(
                Some("file"),
                Some(&parsed.mime_type),
                Some(decoded_bytes(&parsed.base64)),
                "non-image bytes are not inlined into Qoder context",
            )));
        }
        return Some(match ctx.upload_image_data(&parsed.base64, &parsed.mime_type).await {
            UploadOutcome::Url(url) => image_url_block(&url),
            UploadOutcome::Keep => image_url_block(raw),
            UploadOutcome::Stub { bytes } => {
                ctx.stubbed += 1;
                text_block(stub_text(
                    Some("image"),
                    Some(&parsed.mime_type),
                    Some(bytes),
                    "upload failed; not inlined",
                ))
            }
        });
    }

    if block_type == "image" {
        let Some(src) = block.get("source") else { return Some(block.clone()) };
        let src_type = src.get("type").and_then(Json::as_str).unwrap_or("");
        if src_type == "url" {
            if let Some(url) = src.get("url").and_then(Json::as_str) {
                return Some(image_url_block(url));
            }
        }
        if src_type == "base64" {
            if let Some(data) = src.get("data").and_then(Json::as_str).filter(|s| !s.is_empty()) {
                let mime = src.get("media_type").and_then(Json::as_str).unwrap_or("image/png");
                return Some(match ctx.upload_image_data(data, mime).await {
                    UploadOutcome::Url(url) => image_url_block(&url),
                    UploadOutcome::Keep => image_url_block(&format!("data:{mime};base64,{data}")),
                    UploadOutcome::Stub { bytes } => {
                        ctx.stubbed += 1;
                        text_block(stub_text(Some("image"), Some(mime), Some(bytes), "upload failed; not inlined"))
                    }
                });
            }
        }
    }

    if block_type == "file" {
        if let Some(file) = block.get("file") {
            let name = file
                .get("filename")
                .or_else(|| file.get("name"))
                .and_then(Json::as_str)
                .unwrap_or("file");
            let data_uri = file.get("file_data").and_then(Json::as_str);
            let parsed = data_uri.and_then(nr_translator::concerns::image::parse_data_uri);
            let b64: Option<&str> = parsed
                .as_ref()
                .map(|p| p.base64.as_str())
                .or_else(|| data_uri.filter(|d| !d.starts_with("data:")));
            let mime = parsed
                .as_ref()
                .map(|p| p.mime_type.clone())
                .or_else(|| file.get("format").and_then(Json::as_str).map(String::from))
                .unwrap_or_else(|| "application/octet-stream".into());
            if let Some(b64) = b64.filter(|_| is_image_mime(&mime)) {
                if let UploadOutcome::Url(url) = ctx.upload_image_data(b64, &mime).await {
                    return Some(image_url_block(&url));
                }
            }
            ctx.stubbed += 1;
            return Some(text_block(stub_text(
                Some(name),
                Some(&mime),
                Some(decoded_bytes(b64.unwrap_or(""))),
                "Qoder reads documents via its file API, not inlined bytes",
            )));
        }
    }

    if block_type == "document" {
        if let Some(src) = block.get("source") {
            let name = block.get("title").and_then(Json::as_str).unwrap_or("document");
            if src.get("type").and_then(Json::as_str) == Some("base64") {
                if let Some(data) = src.get("data").and_then(Json::as_str).filter(|s| !s.is_empty()) {
                    let mime = src.get("media_type").and_then(Json::as_str).unwrap_or("application/pdf");
                    if is_image_mime(mime) {
                        if let UploadOutcome::Url(url) = ctx.upload_image_data(data, mime).await {
                            return Some(image_url_block(&url));
                        }
                    }
                    ctx.stubbed += 1;
                    return Some(text_block(stub_text(
                        Some(name),
                        Some(mime),
                        Some(decoded_bytes(data)),
                        "Qoder reads documents via its file API, not inlined bytes",
                    )));
                }
            }
        }
    }

    if let Some(text) = block.get("text").and_then(Json::as_str) {
        if text.contains("data:") && text.len() > 8192 {
            let mut next = block.clone();
            next["text"] = Json::String(strip_data_uris(text, ctx));
            return Some(next);
        }
    }

    Some(block.clone())
}

/// Replace over-budget inlined data URIs inside a text blob.
fn strip_data_uris(text: &str, ctx: &mut RewriteCtx<'_>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("data:") {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        // A data URI ends at the first whitespace or quote after the payload.
        let end = tail
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '>' || c == ')')
            .unwrap_or(tail.len());
        let candidate = &tail[..end];
        let parsed = nr_translator::concerns::image::parse_data_uri(candidate);
        let bytes = match &parsed {
            Some(p) => decoded_bytes(&p.base64),
            None => candidate.len(),
        };
        if bytes <= QODER_INLINE_FALLBACK_MAX_BYTES {
            out.push_str(candidate);
        } else {
            ctx.stubbed += 1;
            out.push_str(&stub_text(None, parsed.as_ref().map(|p| p.mime_type.as_str()), Some(bytes), "inlined data URI stripped from Qoder context"));
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// Rewrite one message body's content (string or block array).
async fn rewrite_content(content: &Json, ctx: &mut RewriteCtx<'_>) -> Json {
    match content {
        Json::String(text) => {
            if text.contains("data:") && text.len() > 8192 {
                Json::String(strip_data_uris(text, ctx))
            } else {
                content.clone()
            }
        }
        Json::Array(blocks) => {
            let mut out = Vec::with_capacity(blocks.len());
            for block in blocks {
                if let Some(next) = rewrite_block(block, ctx).await {
                    out.push(next);
                }
            }
            if out.is_empty() {
                Json::String(String::new())
            } else {
                Json::Array(out)
            }
        }
        other => other.clone(),
    }
}

/// `rewriteQoderMessageAttachments(messages, ctx)` — rewrite the messages array
/// in place: upload images, stub huge files, collect surviving image URLs.
pub async fn rewrite_message_attachments(messages: &mut Json, ctx: &mut RewriteCtx<'_>) -> Vec<String> {
    let mut image_urls: Vec<String> = Vec::new();
    let Some(list) = messages.as_array_mut() else { return image_urls };
    if list.is_empty() {
        return image_urls;
    }

    for msg in list.iter_mut() {
        if !msg.is_object() {
            continue;
        }
        // Ollama-style sidecar: fold into content so normalizeMessages sees them.
        if let Some(extras) = msg.get("images").and_then(Json::as_array).cloned() {
            let folded: Vec<Json> = extras
                .iter()
                .filter_map(Json::as_str)
                .map(image_url_block)
                .collect();
            let content = msg.get("content").cloned().unwrap_or(Json::Null);
            let merged = match &content {
                Json::Array(items) => {
                    let mut out = items.clone();
                    out.extend(folded);
                    Json::Array(out)
                }
                Json::String(text) => {
                    let mut out = vec![text_block(text.clone())];
                    out.extend(folded);
                    Json::Array(out)
                }
                _ => Json::Array(folded),
            };
            msg["content"] = merged;
            if let Some(obj) = msg.as_object_mut() {
                obj.shift_remove("images");
            }
        }
        let content = msg.get("content").cloned().unwrap_or(Json::Null);
        let next = rewrite_content(&content, ctx).await;
        msg["content"] = next;
    }

    // Collect surviving http(s) image URLs for callers that want image_urls.
    for msg in list.iter() {
        let Some(blocks) = msg.get("content").and_then(Json::as_array) else { continue };
        for block in blocks {
            let is_image = block.get("type").and_then(Json::as_str) == Some("image_url");
            if is_image {
                let url = match block.get("image_url") {
                    Some(Json::String(s)) => Some(s.clone()),
                    Some(v) => v.get("url").and_then(Json::as_str).map(String::from),
                    None => None,
                };
                if let Some(url) = url {
                    if url.to_lowercase().starts_with("http://") || url.to_lowercase().starts_with("https://") {
                        image_urls.push(url);
                    }
                }
            }
            if block.get("type").and_then(Json::as_str) == Some("text") {
                if let Some(text) = block.get("text").and_then(Json::as_str) {
                    if text.starts_with("[file omitted:") {
                        ctx.stubbed += 1;
                    }
                }
            }
        }
    }
    ctx.uploaded = image_urls.len();

    let payload_size = messages.to_string().len();
    if payload_size > QODER_MAX_PAYLOAD_BYTES {
        ctx.log.warn(
            "QODER",
            format!("request still {payload_size} bytes after rewrite; stripping leftover data URIs"),
        );
        strip_remaining_data_uris(messages, ctx);
    }

    image_urls
}

/// `stripRemainingDataUris` — last-resort size fix.
fn strip_remaining_data_uris(messages: &mut Json, ctx: &mut RewriteCtx<'_>) {
    let Some(list) = messages.as_array_mut() else { return };
    for msg in list.iter_mut() {
        match msg.get("content").cloned() {
            Some(Json::String(text)) if text.contains("data:") => {
                msg["content"] = Json::String(strip_data_uris(&text, ctx));
            }
            Some(Json::Array(blocks)) => {
                let mapped: Vec<Json> = blocks
                    .iter()
                    .map(|block| {
                        if block.get("type").and_then(Json::as_str) == Some("image_url") {
                            let raw = match block.get("image_url") {
                                Some(Json::String(s)) => s.as_str(),
                                Some(v) => v.get("url").and_then(Json::as_str).unwrap_or(""),
                                None => "",
                            };
                            if raw.starts_with("data:") {
                                ctx.stubbed += 1;
                                return text_block(stub_text(
                                    Some("image"),
                                    None,
                                    None,
                                    "payload over Qoder size budget",
                                ));
                            }
                        }
                        if let Some(text) = block.get("text").and_then(Json::as_str) {
                            if text.contains("data:") {
                                let mut next = block.clone();
                                next["text"] = Json::String(strip_data_uris(text, ctx));
                                return next;
                            }
                        }
                        block.clone()
                    })
                    .collect();
                msg["content"] = Json::Array(mapped);
            }
            _ => {}
        }
    }
}

/// Convenience wrapper used by the executor: rewrite with a per-request cache.
pub async fn rewrite_for_request(
    messages: &mut Json,
    credentials: &Credentials,
    log: &Logger,
    proxy: Option<&ProxyOptions>,
    region: &str,
) -> Vec<String> {
    let mut ctx = RewriteCtx::new(credentials, log, proxy, region);
    rewrite_message_attachments(messages, &mut ctx).await
}

/// `clearQoderUploadCache` is a no-op upstream: the memo is per-request now.
pub fn clear_upload_cache() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoded_bytes_matches_base64_expansion() {
        // JS: Math.floor(compact.length * 3 / 4) — padding counts toward length.
        assert_eq!(decoded_bytes("aGVsbG8="), 6);
        assert_eq!(decoded_bytes(""), 0);
        assert_eq!(decoded_bytes("aGVs bG8="), 6);
    }

    #[test]
    fn stub_text_keeps_name_size_and_reason() {
        assert_eq!(
            stub_text(Some("a.png"), Some("image/png"), Some(1234), "upload failed"),
            "[file omitted: a.png, 1234 bytes — upload failed]"
        );
        assert_eq!(stub_text(None, Some("application/pdf"), None, "why"), "[file omitted: application/pdf — why]");
        assert_eq!(stub_text(None, None, None, "why"), "[file omitted: attachment — why]");
    }

    #[test]
    fn extracts_upload_url_from_every_shape() {
        assert_eq!(
            extract_url_from_upload_response(&json!({ "result": { "imageUrls": ["https://oss/x.png"] } })),
            Some("https://oss/x.png".into())
        );
        assert_eq!(
            extract_url_from_upload_response(&json!({ "imageUrls": ["https://oss/y.png"] })),
            Some("https://oss/y.png".into())
        );
        assert_eq!(
            extract_url_from_upload_response(&json!({ "url": "https://oss/z.png" })),
            Some("https://oss/z.png".into())
        );
        assert_eq!(
            extract_url_from_upload_response(&json!({ "body": "{\"ossUrl\":\"https://oss/b.png\"}" })),
            Some("https://oss/b.png".into())
        );
        assert_eq!(extract_url_from_upload_response(&json!({ "nope": 1 })), None);
    }

    #[test]
    fn multipart_body_carries_boundary_and_payload() {
        let (boundary, body) = build_multipart_file(b"abc", "file", "image.png", "image/png");
        let text = String::from_utf8_lossy(&body);
        assert!(text.starts_with(&format!("--{boundary}\r\n")));
        assert!(text.contains("name=\"file\"; filename=\"image.png\""));
        assert!(text.contains("Content-Type: image/png"));
        assert!(text.contains("abc"));
        assert!(text.ends_with(&format!("\r\n--{boundary}--\r\n")));
    }

    #[test]
    fn strips_only_oversized_data_uris() {
        let creds = Credentials::from_value(json!({ "accessToken": "t", "providerSpecificData": { "userId": "u" } }));
        let log = Logger::new();
        let mut ctx = RewriteCtx::new(&creds, &log, None, "intl");
        // 600 KiB payload: over the 512 KiB inline fallback → stubbed.
        let big = "A".repeat(QODER_INLINE_FALLBACK_MAX_BYTES * 2);
        let text = format!("before data:image/png;base64,{big} after");
        let out = strip_data_uris(&text, &mut ctx);
        assert!(out.starts_with("before "));
        assert!(out.contains("[file omitted:"));
        assert!(out.ends_with(" after"));
        assert_eq!(ctx.stubbed, 1);

        // Tiny payload survives untouched.
        let mut ctx = RewriteCtx::new(&creds, &log, None, "intl");
        let small = format!("x data:image/png;base64,{} y", "A".repeat(16));
        assert_eq!(strip_data_uris(&small, &mut ctx), small);
        assert_eq!(ctx.stubbed, 0);
    }

    #[test]
    fn empty_upload_cache_is_a_noop() {
        clear_upload_cache();
    }
}
