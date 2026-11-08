//! Image utilities. Port of open-sse/translator/concerns/image.js.
//! TODO(dedupe): SSRF-hardened fetchImageAsBase64 logic with isPrivateIp DNS checking.

use regex::Regex;
use once_cell::sync::Lazy;

static DATA_URI_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^data:([^;]+);base64,([\s\S]+)$").unwrap()
});

#[derive(Debug, Clone)]
pub struct ParsedDataUri {
    pub mime_type: String,
    pub base64: String,
}

/// Encode a base64 payload as a data URI.
pub fn encode_data_uri(mime_type: &str, base64: &str) -> String {
    format!("data:{};base64,{}", mime_type, base64)
}

/// Parse a base64 data URI → { mime_type, base64 }, or None if not a data URI.
pub fn parse_data_uri(url: &str) -> Option<ParsedDataUri> {
    DATA_URI_RE.captures(url).map(|caps| ParsedDataUri {
        mime_type: caps[1].to_string(),
        base64: caps[2].to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_data_uri() {
        let uri = encode_data_uri("image/png", "iVBORw0KGgo");
        assert_eq!(uri, "data:image/png;base64,iVBORw0KGgo");
    }

    #[test]
    fn test_parse_data_uri_valid() {
        let parsed = parse_data_uri("data:image/png;base64,iVBORw0KGgo");
        assert!(parsed.is_some());
        let p = parsed.unwrap();
        assert_eq!(p.mime_type, "image/png");
        assert_eq!(p.base64, "iVBORw0KGgo");
    }

    #[test]
    fn test_parse_data_uri_invalid() {
        let parsed = parse_data_uri("https://example.com/image.png");
        assert!(parsed.is_none());
    }
}
