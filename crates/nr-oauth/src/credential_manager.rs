//! Port of open-sse/services/oauthCredentialManager.js.
//!
//! FROZEN signature below (executors call it). Wave-0 stub returns `false`
//! until the oauth agent ports it.

use nr_core::Credentials;

/// JS `shouldRefreshCredentials(provider, credentials, nowMs = Date.now())`.
pub fn should_refresh_credentials(_provider: &str, _creds: &Credentials, _now_ms: i64) -> bool {
    false
}
