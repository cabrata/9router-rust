//! Port of open-sse/executors/chatgpt-web/attachments.js
//!
//! ChatGPT Web attachment extraction + resolution (data-URL sources only).

use crate::image_sniff::{sniff_image_dimensions, sniff_image_format, MAX_IMAGE_DECODE_EDGE, MAX_IMAGE_PIXELS};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde_json::Value;

pub const MAX_CHATGPT_WEB_ATTACHMENTS: usize = 10;
pub const MAX_CHATGPT_WEB_IMAGE_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_CHATGPT_WEB_FILE_BYTES: usize = 50 * 1024 * 1024;
pub const MAX_CHATGPT_WEB_TOTAL_ATTACHMENT_BYTES: usize = 50 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ChatGptWebAttachmentError(pub String);

impl std::fmt::Display for ChatGptWebAttachmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ChatGptWebAttachmentError {}

#[derive(Debug, Clone)]
pub struct ResolvedImageAttachment {
    pub kind: String,
    pub name: String,
    pub mime_type: String,
    pub size: usize,
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone)]
pub struct ResolvedFileAttachment {
    pub kind: String,
    pub name: String,
    pub mime_type: String,
    pub size: usize,
    pub data: Vec<u8>,
}

fn optional_string(value: &Value) -> Option<String> {
    value.as_str().and_then(|s| {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn sanitize_filename(value: Option<&str>, fallback: &str) -> String {
    let leaf = value
        .unwrap_or("")
        .split(|c| c == '/' || c == '\\')
        .last()
        .unwrap_or("")
        .chars()
        .filter(|c| *c as u32 > 0x1f && *c as u32 != 0x7f)
        .collect::<String>()
        .trim()
        .to_string();

    let safe = if leaf.is_empty() { fallback.to_string() } else { leaf };
    if safe.len() > 180 {
        safe.chars().take(180).collect()
    } else {
        safe
    }
}

fn mime_from_data_url(ref_url: &str) -> Option<String> {
    let match_str = ref_url
        .strip_prefix("data:")?
        .split(';')
        .next()?
        .trim()
        .to_lowercase();
    if match_str.is_empty() {
        None
    } else {
        Some(match_str)
    }
}

fn extension_for_image_ref(ref_url: &str) -> &'static str {
    if let Some(mime) = mime_from_data_url(ref_url) {
        match mime.as_str() {
            "image/jpeg" | "image/jpg" => return "jpg",
            "image/png" => return "png",
            "image/gif" => return "gif",
            "image/webp" => return "webp",
            _ => {}
        }
    }
    "png"
}

fn decode_data_url(ref_url: &str) -> Result<(Vec<u8>, String), ChatGptWebAttachmentError> {
    let comma = ref_url
        .find(',')
        .ok_or_else(|| ChatGptWebAttachmentError("Attachment data URL is malformed".to_string()))?;

    let header = &ref_url[5..comma];
    if !header.to_lowercase().contains("base64") {
        return Err(ChatGptWebAttachmentError(
            "Attachment data URL must be base64 encoded".to_string(),
        ));
    }

    let mime_type = header.split(';').next().unwrap_or("application/octet-stream").to_lowercase();
    let raw = &ref_url[comma + 1..];

    if raw.len() > MAX_CHATGPT_WEB_FILE_BYTES * 2 {
        return Err(ChatGptWebAttachmentError("Attachment is too large".to_string()));
    }

    let normalized = raw.replace(|c: char| c.is_whitespace(), "");
    if normalized.is_empty() || normalized.len() % 4 != 0 {
        return Err(ChatGptWebAttachmentError(
            "Attachment contains invalid base64 data".to_string(),
        ));
    }

    let bytes = BASE64
        .decode(&normalized)
        .map_err(|_| ChatGptWebAttachmentError("Attachment contains invalid base64 data".to_string()))?;

    if bytes.is_empty() {
        return Err(ChatGptWebAttachmentError(
            "Attachment contains invalid base64 data".to_string(),
        ));
    }

    Ok((bytes, mime_type))
}

fn validate_image(bytes: &[u8], declared_mime: &str) -> Result<(String, u32, u32), ChatGptWebAttachmentError> {
    let format = sniff_image_format(bytes);
    let dimensions = sniff_image_dimensions(bytes);

    let detected_mime = format.map(|fmt| match fmt {
        crate::image_sniff::ImageFormat::Jpeg => "image/jpeg",
        crate::image_sniff::ImageFormat::Png => "image/png",
        crate::image_sniff::ImageFormat::Gif => "image/gif",
        crate::image_sniff::ImageFormat::Webp => "image/webp",
    });

    if detected_mime.is_none() || dimensions.is_none() {
        return Err(ChatGptWebAttachmentError(
            "Image attachment is undecodable or unsupported".to_string(),
        ));
    }

    let detected_mime = detected_mime.unwrap();
    let dims = dimensions.unwrap();

    if declared_mime.starts_with("image/") && declared_mime != detected_mime {
        let jpeg_alias = declared_mime == "image/jpg" && detected_mime == "image/jpeg";
        if !jpeg_alias {
            return Err(ChatGptWebAttachmentError(
                "Image attachment type does not match its data".to_string(),
            ));
        }
    }

    if dims.width > MAX_IMAGE_DECODE_EDGE || dims.height > MAX_IMAGE_DECODE_EDGE
        || dims.width * dims.height > MAX_IMAGE_PIXELS
    {
        return Err(ChatGptWebAttachmentError(
            "Image attachment dimensions are too large".to_string(),
        ));
    }

    Ok((detected_mime.to_string(), dims.width, dims.height))
}

pub async fn resolve_chatgpt_web_attachments(
    sources: Vec<(String, String, String)>,
) -> Result<Vec<ResolvedImageAttachment>, ChatGptWebAttachmentError> {
    if sources.len() > MAX_CHATGPT_WEB_ATTACHMENTS {
        return Err(ChatGptWebAttachmentError(
            format!(
                "ChatGPT Web accepts at most {} attachments per request",
                MAX_CHATGPT_WEB_ATTACHMENTS
            )
        ));
    }

    let mut resolved = Vec::new();
    let mut total_bytes = 0;

    for (kind, name, ref_url) in sources {
        let cap = if kind == "image" {
            MAX_CHATGPT_WEB_IMAGE_BYTES
        } else {
            MAX_CHATGPT_WEB_FILE_BYTES
        };

        if !ref_url.to_lowercase().starts_with("data:") {
            return Err(ChatGptWebAttachmentError(
                "ChatGPT Web attachments must be data URLs".to_string(),
            ));
        }

        let (bytes, mime_type) = decode_data_url(&ref_url)?;

        if bytes.is_empty() {
            return Err(ChatGptWebAttachmentError("Attachment is empty".to_string()));
        }
        if bytes.len() > cap {
            return Err(ChatGptWebAttachmentError("Attachment is too large".to_string()));
        }

        total_bytes += bytes.len();
        if total_bytes > MAX_CHATGPT_WEB_TOTAL_ATTACHMENT_BYTES {
            return Err(ChatGptWebAttachmentError(
                "Combined ChatGPT Web attachments are too large".to_string(),
            ));
        }

        if kind == "image" {
            let (detected_mime, width, height) = validate_image(&bytes, &mime_type)?;
            resolved.push(ResolvedImageAttachment {
                kind: "image".to_string(),
                name,
                mime_type: detected_mime,
                size: bytes.len(),
                data: bytes,
                width,
                height,
            });
        }
    }

    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_filename() {
        assert_eq!(sanitize_filename(Some("test.jpg"), "default"), "test.jpg");
        assert_eq!(sanitize_filename(Some("path/to/file.jpg"), "default"), "file.jpg");
        assert_eq!(sanitize_filename(None, "default"), "default");
    }

    #[test]
    fn test_mime_from_data_url() {
        assert_eq!(mime_from_data_url("data:image/jpeg;base64,abc"), Some("image/jpeg".to_string()));
        assert_eq!(mime_from_data_url("data:image/png;base64,def"), Some("image/png".to_string()));
    }

    #[test]
    fn test_extension_for_image() {
        assert_eq!(extension_for_image_ref("data:image/jpeg;base64,"), "jpg");
        assert_eq!(extension_for_image_ref("data:image/png;base64,"), "png");
        assert_eq!(extension_for_image_ref("unknown"), "png");
    }

    #[test]
    fn test_decode_data_url_invalid() {
        let err = decode_data_url("not-a-data-url");
        assert!(err.is_err());

        let err = decode_data_url("data:text/plain;nobase64,abc");
        assert!(err.is_err());
    }
}
