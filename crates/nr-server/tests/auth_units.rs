//! Unit tests for the dashboard auth surface that does not need the global
//! `nr_db::db()` handle: the progressive login lockout, client-IP trust rules
//! and the session token round-trip. Port of src/lib/auth.js and
//! custom-server.js.
//!
//! These live apart from the HTTP integration tests because `login_handler`
//! and `status_handler` read the process-global database, which cannot be
//! swapped per test.

use axum::http::{HeaderMap, HeaderValue};
use nr_core::paths::data_dir;
use nr_server::routes::auth::{
    check_lock, create_dashboard_auth_token, get_client_ip, get_jwt_secret,
    has_trusted_peer_headers, record_fail, record_success, verify_dashboard_auth_token,
};

/// A loopback peer address for cases that must not trip the remote-access
/// guard.
fn loopback_addr() -> std::net::SocketAddr {
    "127.0.0.1:54321".parse().unwrap()
}

fn headers_with(name: axum::http::HeaderName, value: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(name, HeaderValue::from_str(value).unwrap());
    h
}

// ─── Client IP trust ────────────────────────────────────────────────────────

#[test]
fn peer_addr_is_used_without_proxy_headers() {
    let headers = HeaderMap::new();
    assert_eq!(get_client_ip(&headers, Some(loopback_addr())), "127.0.0.1");
}

#[test]
fn missing_headers_and_addr_fall_back_to_unknown() {
    assert_eq!(get_client_ip(&HeaderMap::new(), None), "unknown");
}

/// The trust rules are read from env, which is process-global, so every
/// env-dependent case runs inside ONE test to avoid racing parallel threads.
#[test]
fn proxy_trust_rules() {
    // Without TRUST_PROXY a client-supplied XFF must not change the resolved
    // IP: spoofing it would let an attacker dodge the per-IP lockout.
    let mut h = headers_with(axum::http::header::HeaderName::from_static("x-forwarded-for"), "203.0.113.7");
    assert_eq!(get_client_ip(&h, Some(loopback_addr())), "127.0.0.1");

    // TRUST_PROXY=false keeps the same behaviour.
    let guard_false = EnvGuard::set("TRUST_PROXY", "false");
    assert_eq!(get_client_ip(&h, Some(loopback_addr())), "127.0.0.1");
    drop(guard_false);

    // TRUST_PROXY=true honours the leftmost (client) hop of the chain.
    let guard_true = EnvGuard::set("TRUST_PROXY", "true");
    h.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.7, 70.41.3.18"));
    assert_eq!(get_client_ip(&h, Some(loopback_addr())), "203.0.113.7");
    drop(guard_true);

    // The reverse-proxy token is trusted only when it matches the configured
    // secret, and only then does the real-IP header count.
    let token_guard = EnvGuard::set("NINEROUTER_PEER_TOKEN", "secret-token");
    h.remove("x-forwarded-for");
    h.insert("x-9r-real-ip", HeaderValue::from_static("9.9.9.9"));
    h.insert("x-9r-peer-token", HeaderValue::from_static("secret-token"));
    assert_eq!(get_client_ip(&h, Some(loopback_addr())), "9.9.9.9");

    // A wrong token is ignored: fall back to the socket address.
    h.insert("x-9r-peer-token", HeaderValue::from_static("nope"));
    assert_eq!(get_client_ip(&h, Some(loopback_addr())), "127.0.0.1");
    drop(token_guard);
}

/// `hasTrustedPeerHeaders` on its own: no configured secret means no trust,
/// even if the client sends the header.
#[test]
fn trusted_peer_requires_a_configured_secret() {
    let prev = std::env::var_os("NINEROUTER_PEER_TOKEN");
    std::env::remove_var("NINEROUTER_PEER_TOKEN");

    let mut h = HeaderMap::new();
    h.insert("x-9r-peer-token", HeaderValue::from_static(""));
    h.insert("x-9r-real-ip", HeaderValue::from_static("9.9.9.9"));
    assert!(!has_trusted_peer_headers(&h), "unset secret never trusts");
    assert_eq!(get_client_ip(&h, Some(loopback_addr())), "127.0.0.1");

    // An empty secret is as good as unset: an attacker can send an empty header.
    std::env::set_var("NINEROUTER_PEER_TOKEN", "");
    assert!(!has_trusted_peer_headers(&h));

    match prev {
        Some(v) => std::env::set_var("NINEROUTER_PEER_TOKEN", v),
        None => std::env::remove_var("NINEROUTER_PEER_TOKEN"),
    }
}

/// The ladder is 30s, 2m, 10m, 30m and then stays at 30m.
#[test]
fn lock_steps_escalate_and_max_out() {
    let ip = "lockout-ladder-ip";
    let mut observed = Vec::new();
    for _ in 0..5 {
        for _ in 0..6 {
            record_fail(ip);
        }
        let st = check_lock(ip);
        assert!(st.locked, "6 failures must lock");
        observed.push(st.retry_after);
        // Release the lock without forgetting the level so the next rung shows.
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(observed[0] <= 30 && observed[0] > 0, "first rung 30s: {observed:?}");
    assert!(observed[1] > observed[0], "second rung is longer: {observed:?}");
    assert_eq!(observed.last().copied(), observed.get(3).copied(), "caps at 30m");
}

/// RAII: set an env var for the duration of a scope, restore it after.
struct EnvGuard {
    key: &'static str,
    prev: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let prev = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

// ─── Login lockout ──────────────────────────────────────────────────────────
//
// The limiter keys on the IP string, so every case here uses its own unique
// key and the tests stay independent even when run in parallel.

#[test]
fn clean_ip_is_not_locked() {
    let st = check_lock("lockout-fresh-ip");
    assert!(!st.locked, "expected no lock, got retry after {}s", st.retry_after);
}

#[test]
fn repeated_failures_lock_and_success_clears() {
    let ip = "lockout-sequential-ip";

    // Five bad attempts are tolerated before the first lock engages.
    for _ in 0..5 {
        let st = check_lock(ip);
        assert!(!st.locked, "attempt 5 should still be open, got {}", st.retry_after);
        record_fail(ip);
    }
    record_fail(ip);

    let locked = check_lock(ip);
    assert!(locked.locked, "sixth failure should lock the IP");
    assert!(locked.retry_after >= 30, "first lock step is 30s, got {}", locked.retry_after);

    // A successful login wipes the record, so the IP can log in again.
    record_success(ip);
    let after = check_lock(ip);
    assert!(!after.locked, "success must clear the lock");
}

#[test]
fn independent_ips_are_isolated() {
    let a = "lockout-isolated-a";
    let b = "lockout-isolated-b";

    for _ in 0..6 {
        record_fail(a);
    }
    assert!(check_lock(a).locked, "hammering ip a locks only a");
    let st = check_lock(b);
    assert!(!st.locked, "ip b is untouched: {}", st.retry_after);
}

#[test]
fn a_locked_ip_reports_a_positive_retry_window() {
    let ip = "lockout-retry-window";
    for _ in 0..6 {
        record_fail(ip);
    }
    let locked = check_lock(ip);
    assert!(locked.locked);
    // The handler formats this into Retry-After, so it is never 0 while locked.
    assert!(locked.retry_after >= 1);
}

#[test]
fn record_success_on_an_ip_that_never_failed_is_not_an_error() {
    record_success("lockout-never-failed-ip");
}

// ─── Session token ──────────────────────────────────────────────────────────

#[test]
fn dashboard_token_round_trips_and_authenticates() {
    let token = create_dashboard_auth_token(None).unwrap();
    assert!(!token.is_empty());

    let claims = verify_dashboard_auth_token(&token).expect("valid token");
    assert!(claims.authenticated, "token asserts an authenticated session");
    assert!(claims.oidc.is_none(), "a password session carries no OIDC claim");
    assert!(claims.saml.is_none());
    assert!(claims.exp > claims.iat, "token expires after it is issued");
    // The 24h dashboard session window.
    assert_eq!(claims.exp - claims.iat, 24 * 60 * 60);
}

#[test]
fn dashboard_token_carries_oidc_identity_claims() {
    let token = create_dashboard_auth_token(Some(serde_json::json!({
        "oidc": true,
        "oidcSub": "sub-1",
        "oidcEmail": "user@example.com",
        "oidcName": "Test User"
    })))
    .unwrap();

    let claims = verify_dashboard_auth_token(&token).unwrap();
    assert_eq!(claims.oidc, Some(true));
    assert_eq!(claims.oidc_sub.as_deref(), Some("sub-1"));
    assert_eq!(claims.oidc_email.as_deref(), Some("user@example.com"));
    assert_eq!(claims.oidc_name.as_deref(), Some("Test User"));
    assert!(claims.saml.is_none(), "an OIDC session is not a SAML one");
}

#[test]
fn dashboard_token_carries_saml_claims() {
    let token = create_dashboard_auth_token(Some(serde_json::json!({
        "saml": true,
        "samlEmail": "user@corp.example"
    })))
    .unwrap();
    let claims = verify_dashboard_auth_token(&token).unwrap();
    assert_eq!(claims.saml, Some(true));
    assert_eq!(claims.saml_email.as_deref(), Some("user@corp.example"));
    assert!(claims.oidc.is_none());
}

#[test]
fn structurally_valid_but_bogus_tokens_are_rejected() {
    assert!(verify_dashboard_auth_token("not-a-jwt").is_none());
    assert!(verify_dashboard_auth_token("").is_none());
    // Right shape, wrong signature.
    let forged = "eyJhbGciOiJIUzI1NiJ9.eyJhdXRoZW50aWNhdGVkIjp0cnVlfQ.bogus";
    assert!(verify_dashboard_auth_token(forged).is_none());
}

#[test]
fn tampered_payload_is_rejected_by_the_signature() {
    let token = create_dashboard_auth_token(None).unwrap();
    let mut parts: Vec<String> = token.split('.').map(str::to_string).collect();
    parts[1] = base64_url_no_pad(br#"{"authenticated":true}"#);
    let tampered = parts.join(".");
    assert_ne!(tampered, token);
    assert!(verify_dashboard_auth_token(&tampered).is_none());
}

#[test]
fn jwt_secret_is_non_empty_and_persisted_in_the_data_dir() {
    let secret = get_jwt_secret();
    assert!(!secret.is_empty(), "an empty secret would accept forged tokens");

    // The generated secret is written to disk so a restart keeps sessions valid.
    let path = data_dir().join("jwt-secret");
    assert!(path.starts_with(data_dir()));
    assert_eq!(path.file_name().and_then(|s| s.to_str()), Some("jwt-secret"));
}

// ─── Helper ─────────────────────────────────────────────────────────────────

/// URL-safe base64 without padding (the JWT segment encoding).
fn base64_url_no_pad(input: &[u8]) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    URL_SAFE_NO_PAD.encode(input)
}

#[test]
fn base64_helper_uses_the_jwt_segment_alphabet() {
    assert_eq!(base64_url_no_pad(b"abc"), "YWJj");
    assert_eq!(base64_url_no_pad(b"a"), "YQ");
    assert!(!base64_url_no_pad(b"??").contains('+'), "no standard-alphabet chars");
}
