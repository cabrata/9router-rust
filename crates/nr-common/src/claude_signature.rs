//! Port of open-sse/utils/claudeSignature.js.
//!
//! Claude thinking signature validation. `E`-form: single-layer base64 whose decoded
//! first byte is the Claude marker `0x12`. `R`-form: double-layer base64 — outer
//! decodes to an ASCII string starting with `E`, inner decodes to the marker.
//! A `"...#sig"` cache prefix is stripped before validation.

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};

const MAX_CLAUDE_SIGNATURE_LEN: usize = 32 * 1024 * 1024;
const CLAUDE_SIGNATURE_MARKER: u8 = 0x12;

fn strip_cache_prefix(raw_signature: &str) -> &str {
    let sig = raw_signature.trim();
    if sig.is_empty() {
        return "";
    }
    match sig.find('#') {
        Some(idx) => sig[idx + 1..].trim(),
        None => sig,
    }
}

/// `hasClaudeSignaturePrefix` — non-empty and starts with `E` or `R`.
pub fn has_claude_signature_prefix(raw_signature: &str) -> bool {
    let sig = strip_cache_prefix(raw_signature);
    !sig.is_empty() && (sig.starts_with('E') || sig.starts_with('R'))
}

/// `isValidClaudeSignature` — validates the base64 layer(s) plus the Claude marker byte.
pub fn is_valid_claude_signature(raw_signature: &str) -> bool {
    let sig = strip_cache_prefix(raw_signature);
    if sig.is_empty() || sig.len() > MAX_CLAUDE_SIGNATURE_LEN {
        return false;
    }

    if sig.starts_with('E') {
        // JS Buffer.from ignores invalid chars; STANDARD errors, so treat those as invalid.
        return BASE64
            .decode(sig)
            .is_ok_and(|d| !d.is_empty() && d[0] == CLAUDE_SIGNATURE_MARKER);
    }
    if sig.starts_with('R') {
        let Ok(outer) = BASE64.decode(sig) else { return false };
        if outer.is_empty() || outer[0] != b'E' {
            return false;
        }
        let Ok(outer_str) = std::str::from_utf8(&outer) else { return false };
        let Ok(inner) = BASE64.decode(outer_str) else { return false };
        return !inner.is_empty() && inner[0] == CLAUDE_SIGNATURE_MARKER;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn b64(bytes: &[u8]) -> String {
        BASE64.encode(bytes)
    }

    #[test]
    fn valid_e_form() {
        let sig = b64(&[0x12, 0xaa, 0xbb]);
        assert!(has_claude_signature_prefix(&sig));
        assert!(is_valid_claude_signature(&sig));
    }

    #[test]
    fn valid_r_form() {
        // R-form: outer base64 decodes to ASCII text starting with 'E' whose own base64
        // decodes to the marker byte. b64([0x12]) is "Eg==", which already starts with E.
        let inner_text = b64(&[0x12]);
        assert!(inner_text.starts_with('E'));
        let outer = b64(inner_text.as_bytes());
        assert!(outer.starts_with('R'));
        assert!(has_claude_signature_prefix(&outer));
        assert!(is_valid_claude_signature(&outer));
    }

    #[test]
    fn r_form_with_wrong_outer_byte_is_invalid() {
        // Outer decoded text must itself start with 'E' (0x45).
        let outer = b64(b"XEg==");
        assert!(!is_valid_claude_signature(&outer));
    }

    #[test]
    fn cache_prefix_is_stripped() {
        // Cache keys are stored as `<cacheId>#<signature>`; JS slices after the '#'.
        let sig = b64(&[0x12]);
        let prefixed = format!("cachekey#{sig}");
        assert!(has_claude_signature_prefix(&prefixed));
        assert!(is_valid_claude_signature(&prefixed));
    }

    #[test]
    fn wrong_marker_is_invalid() {
        assert!(!is_valid_claude_signature(&b64(&[0x00, 0x01])));
    }

    #[test]
    fn garbage_and_empty_are_invalid() {
        assert!(!is_valid_claude_signature(""));
        assert!(!is_valid_claude_signature("!!!not base64!!!"));
        assert!(!has_claude_signature_prefix(""));
    }
}
