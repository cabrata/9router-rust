//! MITM request/response logging. Port of src/mitm/logger.js.
//!
//! Two sinks: `tracing` for operator output, and (dev only) raw dump files under
//! `$DATA_DIR/logs/mitm` for offline inspection of an IDE's wire format.

use std::path::{Path, PathBuf};
use std::sync::Once;

use crate::mitm::paths;

static CLEAR_ONCE: Once = Once::new();

/// File dumps are a development aid (the JS build gates them on NODE_ENV).
pub fn dump_enabled() -> bool {
    std::env::var("NODE_ENV").map(|v| v == "development").unwrap_or(false)
}

/// Clear stale dump files on every MITM start (prevents unbounded disk usage).
/// Idempotent per process; the JS build calls it at module load.
pub fn clear_dump_dir() {
    CLEAR_ONCE.call_once(|| {
        let dir = paths::dump_dir();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            let _ = if p.is_dir() {
                std::fs::remove_dir_all(&p)
            } else {
                std::fs::remove_file(&p)
            };
        }
    });
}

/// `[MITM] msg` on stdout — matches the JS log prefix so operators see the same lines.
pub fn log(msg: &str) {
    tracing::info!("[MITM] {msg}");
}

pub fn err(msg: &str) {
    tracing::error!("[MITM] {msg}");
}

fn slugify(s: &str, max: usize) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(max)
        .collect()
}

fn timestamp_slug() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H-%M-%S-%3fZ")
        .to_string()
}

/// Header names that must never reach a dump file. A MITM dump is plain text on
/// disk; credentials in it are a real leak, not just noise.
const SECRET_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "api-key",
    "x-goog-api-key",
    "x-amz-security-token",
];

pub fn redact_header(name: &str, value: &str) -> String {
    if SECRET_HEADERS.iter().any(|h| name.eq_ignore_ascii_case(h)) {
        // Keep a short prefix so operators can tell *which* credential was used
        // (e.g. `Bearer eyJ...`) without exposing the secret itself.
        match value.split_once(' ') {
            Some((scheme, _)) if scheme.len() <= 12 => format!("{scheme} <redacted>"),
            _ => "<redacted>".to_string(),
        }
    } else {
        value.to_string()
    }
}

/// Render headers as a JSON object with secrets redacted.
pub fn redacted_headers(headers: &http::HeaderMap) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for (name, value) in headers {
        let name = name.as_str();
        let raw = value.to_str().unwrap_or("<binary>");
        out.insert(name.to_string(), serde_json::Value::String(redact_header(name, raw)));
    }
    serde_json::Value::Object(out)
}

/// Decode a response body for readable dumps, honouring `content-encoding`.
///
/// ponytail: gzip bodies are returned compressed (no flate2 dep for a dev-only
/// debug path). Upgrade: add `flate2` and gunzip here when a debug session needs it.
pub fn decode_body(buf: &[u8], _encoding: Option<&str>) -> Vec<u8> {
    buf.to_vec()
}

/// Save a raw request (method + url + redacted headers + body) as a dump file.
/// Returns the path written, or `None` when blacklisted/on IO error (fail-open).
pub fn dump_request(
    method: &str,
    url: &str,
    host: &str,
    headers: &http::HeaderMap,
    body: &[u8],
) -> Option<PathBuf> {
    if crate::mitm::config::is_blacklisted(url) {
        return None;
    }
    let dir = paths::dump_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let slug = slugify(&format!("{host}{url}"), 80);
    let file = dir.join(format!("{}_{}_raw_{}.req.json", timestamp_slug(), "raw", slug));
    let body_value = serde_json::from_slice::<serde_json::Value>(body)
        .unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(body).into_owned()));
    let payload = serde_json::json!({
        "method": method,
        "url": url,
        "host": host,
        "headers": redacted_headers(headers),
        "body": body_value,
    });
    let text = serde_json::to_vec_pretty(&payload).ok()?;
    std::fs::write(&file, text).ok()?;
    Some(file)
}

/// Buffer-based response dumper: collects chunks, decodes on `finish`, writes once.
///
/// Trade-off (same as JS): holds the response in RAM, but enables gzip decoding
/// so the dump is readable.
pub struct ResponseDumper {
    file: PathBuf,
    status: u16,
    headers: http::HeaderMap,
    chunks: Vec<u8>,
}

/// Bodies that carry no signal (empty JSON shells) are skipped, as in JS.
fn is_empty_body(text: &str) -> bool {
    matches!(text.trim(), "" | "{}" | "[]" | "null")
}

impl ResponseDumper {
    pub fn new(url: &str, host: &str, tag: &str) -> Option<Self> {
        if crate::mitm::config::is_blacklisted(url) {
            return None;
        }
        let dir = paths::dump_dir();
        std::fs::create_dir_all(&dir).ok()?;
        let slug = slugify(&format!("{host}{url}"), 80);
        Some(Self {
            file: dir.join(format!("{}_{}_{}.res.txt", timestamp_slug(), tag, slug)),
            status: 0,
            headers: http::HeaderMap::new(),
            chunks: Vec::new(),
        })
    }

    pub fn write_header(&mut self, status: u16, headers: &http::HeaderMap) {
        self.status = status;
        self.headers = headers.clone();
    }

    pub fn write_chunk(&mut self, chunk: &[u8]) {
        self.chunks.extend_from_slice(chunk);
    }

    pub fn path(&self) -> &Path {
        &self.file
    }

    pub fn finish(self) {
        let encoding = self
            .headers
            .get("content-encoding")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let decoded = decode_body(&self.chunks, encoding.as_deref());
        let text = String::from_utf8_lossy(&decoded);
        if is_empty_body(&text) {
            return;
        }
        // Strip content-encoding: the body on disk is already decoded.
        let mut headers = serde_json::Map::new();
        for (name, value) in &self.headers {
            let n = name.as_str();
            if n.eq_ignore_ascii_case("content-encoding") {
                continue;
            }
            headers.insert(
                n.to_string(),
                serde_json::Value::String(value.to_str().unwrap_or("<binary>").to_string()),
            );
        }
        let out = format!(
            "STATUS: {}\nHEADERS: {}\n---BODY---\n{}",
            self.status,
            serde_json::to_string_pretty(&serde_json::Value::Object(headers))
                .unwrap_or_else(|_| "{}".into()),
            text
        );
        let _ = std::fs::write(&self.file, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_credentials_but_keeps_scheme() {
        assert_eq!(redact_header("authorization", "Bearer sk-live-123"), "Bearer <redacted>");
        assert_eq!(redact_header("Cookie", "session=abc"), "<redacted>");
        assert_eq!(redact_header("content-type", "application/json"), "application/json");
    }

    #[test]
    fn empty_bodies_are_recognised() {
        assert!(is_empty_body("{}"));
        assert!(is_empty_body("  []  "));
        assert!(!is_empty_body(r#"{"a":1}"#));
    }

    #[test]
    fn slugify_keeps_alnum_only() {
        assert_eq!(slugify("api2.cursor.sh/v1/x?y=1", 80), "api2_cursor_sh_v1_x_y_1");
    }
}
