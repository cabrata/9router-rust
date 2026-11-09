//! OAuth utilities: PKCE, state generation, URL validation, etc.
//! Ports src/lib/oauth/utils/pkce.js and related helpers.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::Rng;
use sha2::{Digest, Sha256};

/// Generate PKCE code verifier (43-128 characters, base64url-encoded random bytes).
/// Default: 32 bytes → 43-44 characters.
pub fn generate_code_verifier(bytes: usize) -> String {
    let mut rng = rand::thread_rng();
    let mut buf = vec![0u8; bytes];
    rng.fill(&mut buf[..]);
    URL_SAFE_NO_PAD.encode(&buf)
}

/// Generate PKCE code challenge from verifier using SHA256 (S256 method).
pub fn generate_code_challenge(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

/// Generate random state for CSRF protection (32 bytes base64url).
pub fn generate_state() -> String {
    let mut rng = rand::thread_rng();
    let mut buf = vec![0u8; 32];
    rng.fill(&mut buf[..]);
    URL_SAFE_NO_PAD.encode(&buf)
}

/// Generate complete PKCE pair with state.
pub fn generate_pkce(verifier_bytes: usize) -> (String, String, String) {
    let verifier = generate_code_verifier(verifier_bytes);
    let challenge = generate_code_challenge(&verifier);
    let state = generate_state();
    (verifier, challenge, state)
}

/// Generate nonce (for OpenID Connect).
pub fn generate_nonce() -> String {
    let mut rng = rand::thread_rng();
    let mut buf = vec![0u8; 16];
    rng.fill(&mut buf[..]);
    hex::encode(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_code_verifier() {
        let verifier = generate_code_verifier(32);
        assert!(verifier.len() >= 43 && verifier.len() <= 128);
        // Valid base64url (no +, /, or =)
        assert!(!verifier.contains('+'));
        assert!(!verifier.contains('/'));
        assert!(!verifier.contains('='));
    }

    #[test]
    fn test_generate_code_challenge() {
        let verifier = "test_verifier_string_at_least_43_chars_long_for_pkce";
        let challenge = generate_code_challenge(verifier);
        // Challenge should be base64url-encoded SHA256 (~43 chars)
        assert!(!challenge.contains('+'));
        assert!(!challenge.contains('/'));
        assert!(!challenge.contains('='));
        // SHA256 output is 32 bytes = 43 chars in base64url
        assert_eq!(challenge.len(), 43);
    }

    #[test]
    fn test_generate_state() {
        let state = generate_state();
        // 32 bytes base64url = 43 chars
        assert_eq!(state.len(), 43);
        assert!(!state.contains('+'));
        assert!(!state.contains('/'));
    }

    #[test]
    fn test_generate_pkce() {
        let (verifier, challenge, state) = generate_pkce(32);
        assert!(!verifier.is_empty());
        assert_eq!(challenge.len(), 43); // SHA256 in base64url
        assert_eq!(state.len(), 43);
        // Challenge should be deterministic for the same verifier
        let challenge2 = generate_code_challenge(&verifier);
        assert_eq!(challenge, challenge2);
    }

    #[test]
    fn test_generate_nonce() {
        let nonce = generate_nonce();
        // 16 bytes hex = 32 chars
        assert_eq!(nonce.len(), 32);
        assert!(nonce.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
