//! Image utilities. Port of open-sse/translator/concerns/image.js.

use std::net::IpAddr;
use std::time::Duration;

use base64::Engine;
use once_cell::sync::Lazy;
use regex::Regex;

static DATA_URI_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^data:([^;]+);base64,([\s\S]+)$").unwrap());

pub const DEFAULT_MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024; // 20 MiB
pub const DEFAULT_FETCH_TIMEOUT_MS: u64 = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
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

/// True if an IPv4/IPv6 address is private/reserved (SSRF target).
pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            // loopback (127.0.0.0/8), 0.0.0.0/8
            if o[0] == 127 || o[0] == 0 {
                return true;
            }
            // 10.0.0.0/8
            if o[0] == 10 {
                return true;
            }
            // 172.16.0.0/12
            if o[0] == 172 && (16..=31).contains(&o[1]) {
                return true;
            }
            // 192.168.0.0/16
            if o[0] == 192 && o[1] == 168 {
                return true;
            }
            // 169.254.0.0/16 (link-local, cloud metadata)
            if o[0] == 169 && o[1] == 254 {
                return true;
            }
            // 100.64.0.0/10 (CGNAT)
            if o[0] == 100 && (64..=127).contains(&o[1]) {
                return true;
            }
            // broadcast / multicast
            if o[0] >= 224 {
                return true;
            }
            false
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() {
                return true;
            }
            let s = v6.segments();
            // IPv4-mapped IPv6 ::ffff:a.b.c.d
            if s[0] == 0 && s[1] == 0 && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0xffff {
                let v4 = std::net::Ipv4Addr::new(
                    (s[6] >> 8) as u8,
                    (s[6] & 0xff) as u8,
                    (s[7] >> 8) as u8,
                    (s[7] & 0xff) as u8,
                );
                return is_private_ip(IpAddr::V4(v4));
            }
            let first = (s[0] >> 8) as u8;
            // unique-local (fc00::/7) or link-local (fe80::/10)
            if (first & 0xfe) == 0xfc || (first == 0xfe && (s[0] & 0xc0) == 0x80) {
                return true;
            }
            false
        }
    }
}

/// Known image signatures: offset, magic bytes, MIME, verify_webp flag.
struct Sig {
    offset: usize,
    bytes: &'static [u8],
    mime: &'static str,
    verify_webp: bool,
}

static SIGNATURES: &[Sig] = &[
    Sig { offset: 0, bytes: &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A], mime: "image/png", verify_webp: false },
    Sig { offset: 0, bytes: &[0xFF, 0xD8, 0xFF], mime: "image/jpeg", verify_webp: false },
    Sig { offset: 0, bytes: b"GIF87a", mime: "image/gif", verify_webp: false },
    Sig { offset: 0, bytes: b"GIF89a", mime: "image/gif", verify_webp: false },
    Sig { offset: 0, bytes: b"RIFF", mime: "image/webp", verify_webp: true },
    Sig { offset: 0, bytes: b"BM", mime: "image/bmp", verify_webp: false },
];

/// Verify buffer magic bytes match a known image signature.
pub fn detect_image_mime(buf: &[u8]) -> Option<&'static str> {
    for sig in SIGNATURES {
        let end = sig.offset + sig.bytes.len();
        if buf.len() < end {
            continue;
        }
        if &buf[sig.offset..end] != sig.bytes {
            continue;
        }
        if sig.verify_webp {
            // RIFF....WEBP: bytes 8..12 must be "WEBP"
            if buf.len() < 12 || &buf[8..12] != b"WEBP" {
                continue;
            }
        }
        return Some(sig.mime);
    }
    None
}

#[derive(Clone, Debug)]
pub struct FetchImageResult {
    pub url: String,
    pub mime_type: String,
}

#[derive(Clone, Debug)]
pub struct FetchOptions {
    pub timeout_ms: u64,
    pub max_bytes: usize,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            timeout_ms: DEFAULT_FETCH_TIMEOUT_MS,
            max_bytes: DEFAULT_MAX_IMAGE_BYTES,
        }
    }
}

/// Fetch a remote image URL and return it as a base64 data URI.
/// Hardened against SSRF (private IPs), memory DoS (size cap), and disguised payloads (magic bytes).
pub async fn fetch_image_as_base64(image_url: &str, options: &FetchOptions) -> Option<FetchImageResult> {
    if !image_url.starts_with("http://") && !image_url.starts_with("https://") {
        return None;
    }
    // shortcut: full asynchronous DNS resolution + pinned socket is handled in nr-core::proxy_fetch;
    // here we do standard SSRF check on parsed hostname.
    let parsed = reqwest::Url::parse(image_url).ok()?;
    let host = parsed.host_str()?;
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_private_ip(ip) {
            return None;
        }
    } else {
        // Resolve host
        let addrs = tokio::net::lookup_host((host, parsed.port_or_known_default().unwrap_or(80))).await.ok()?;
        let ips: Vec<IpAddr> = addrs.map(|a| a.ip()).collect();
        if ips.is_empty() || ips.iter().any(|ip| is_private_ip(*ip)) {
            return None;
        }
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(options.timeout_ms))
        .redirect(reqwest::redirect::Policy::none()) // Prevent SSRF redirect
        .build()
        .ok()?;

    let response = client.get(image_url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }

    let bytes = response.bytes().await.ok()?;
    if bytes.len() > options.max_bytes {
        return None;
    }
    let mime_type = detect_image_mime(&bytes)?.to_string();
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let url = encode_data_uri(&mime_type, &b64);
    Some(FetchImageResult { url, mime_type })
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
        let parsed = parse_data_uri("data:image/png;base64,iVBORw0KGgo").unwrap();
        assert_eq!(parsed.mime_type, "image/png");
        assert_eq!(parsed.base64, "iVBORw0KGgo");
    }

    #[test]
    fn test_parse_data_uri_invalid() {
        assert!(parse_data_uri("https://example.com/image.png").is_none());
    }

    #[test]
    fn test_is_private_ip() {
        assert!(is_private_ip("127.0.0.1".parse().unwrap()));
        assert!(is_private_ip("10.0.0.1".parse().unwrap()));
        assert!(is_private_ip("172.16.0.1".parse().unwrap()));
        assert!(is_private_ip("192.168.1.1".parse().unwrap()));
        assert!(is_private_ip("169.254.169.254".parse().unwrap()));
        assert!(is_private_ip("::1".parse().unwrap()));
        assert!(!is_private_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_private_ip("1.1.1.1".parse().unwrap()));
    }

    #[test]
    fn test_detect_image_mime_png() {
        let png = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00];
        assert_eq!(detect_image_mime(&png), Some("image/png"));
    }

    #[test]
    fn test_detect_image_mime_jpeg() {
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 0x00];
        assert_eq!(detect_image_mime(&jpeg), Some("image/jpeg"));
    }

    #[test]
    fn test_detect_image_mime_webp() {
        let mut webp = b"RIFF1234WEBPextra".to_vec();
        assert_eq!(detect_image_mime(&webp), Some("image/webp"));
        webp[8] = b'X';
        assert_eq!(detect_image_mime(&webp), None);
    }
}
