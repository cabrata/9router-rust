/// Shared SSE primitives (no imports — safe for executors + stream.js)

pub const SSE_DONE: &str = "data: [DONE]\n\n";

pub const SSE_HEADERS: &[(&str, &str)] = &[
    ("Content-Type", "text/event-stream"),
    ("Cache-Control", "no-cache"),
    ("Connection", "keep-alive"),
];

/// Variant for web-cookie executors behind nginx (disable proxy buffering)
pub const SSE_HEADERS_NO_BUFFER: &[(&str, &str)] = &[
    ("Content-Type", "text/event-stream"),
    ("Cache-Control", "no-cache"),
    ("X-Accel-Buffering", "no"),
];

/// Variant for client-facing SSE responses (adds permissive CORS)
pub const SSE_HEADERS_CORS: &[(&str, &str)] = &[
    ("Content-Type", "text/event-stream"),
    ("Cache-Control", "no-cache"),
    ("Connection", "keep-alive"),
    ("Access-Control-Allow-Origin", "*"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sse_done() {
        assert_eq!(SSE_DONE, "data: [DONE]\n\n");
    }

    #[test]
    fn test_sse_headers_has_content_type() {
        assert!(SSE_HEADERS.iter().any(|(k, _)| *k == "Content-Type"));
    }
}
