//! SSRF guard for user-supplied URLs. Port of src/shared/utils/ssrfGuard.js
//! (`assertPublicUrl`, `assertPublicUrlResolved`).
//!
//! Blocks loopback, private, CGNAT and cloud-metadata targets, including
//! hostnames that merely resolve to one (DNS lookup, not just literals).

use std::net::IpAddr;

const BLOCKED_HOSTNAMES: &[&str] = &["localhost", "ip6-localhost", "ip6-loopback"];
const BLOCKED_SUFFIXES: &[&str] = &[".internal", ".local", ".localhost"];

/// Private/reserved IPv4 ranges as `(network, prefix_len)`.
const BLOCKED_V4_RANGES: &[( [u8; 4], u32 )] = &[
    ([0, 0, 0, 0], 8),
    ([10, 0, 0, 0], 8),
    ([100, 64, 0, 0], 10),   // CGNAT — also used by some cloud metadata proxies
    ([127, 0, 0, 0], 8),
    ([169, 254, 0, 0], 16),  // includes 169.254.169.254 cloud metadata
    ([172, 16, 0, 0], 12),
    ([192, 168, 0, 0], 16),
];

fn ipv4_to_u32(ip: [u8; 4]) -> u32 {
    u32::from_be_bytes(ip)
}

fn is_blocked_ipv4(ip: [u8; 4]) -> bool {
    let value = ipv4_to_u32(ip);
    BLOCKED_V4_RANGES.iter().any(|(base, bits)| {
        let mask = if *bits == 0 { 0 } else { u32::MAX << (32 - bits) };
        (value & mask) == (ipv4_to_u32(*base) & mask)
    })
}

/// Blocked IPv6: loopback, unspecified, link-local, unique-local, and
/// IPv4-mapped addresses pointing at a blocked IPv4 range.
fn is_blocked_ipv6(ip: std::net::Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() {
        return true;
    }
    let segs = ip.segments();
    // fc00::/7 unique-local
    if segs[0] & 0xfe00 == 0xfc00 {
        return true;
    }
    // fe80::/10 link-local
    if segs[0] & 0xffc0 == 0xfe80 {
        return true;
    }
    // ::ffff:a.b.c.d IPv4-mapped
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_blocked_ipv4(v4.octets());
    }
    false
}

/// Literal-only check: `true` when the URL host is a blocked name or IP.
/// Port of JS `isBlockedUrl`/`assertPublicUrl` literal half.
pub fn is_blocked_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return true;
    }
    if BLOCKED_HOSTNAMES.contains(&host.as_str()) {
        return true;
    }
    if BLOCKED_SUFFIXES.iter().any(|s| host.ends_with(s)) {
        return true;
    }
    // Bracketed IPv6 literal, e.g. [::1]
    let inner = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = inner.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(v4) => is_blocked_ipv4(v4.octets()),
            IpAddr::V6(v6) => is_blocked_ipv6(v6),
        };
    }
    false
}

/// Validate the URL literal (no DNS). Port of JS `assertPublicUrl`.
pub fn assert_public_url(raw_url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(raw_url).map_err(|_| "Invalid URL format".to_string())?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(format!("Blocked URL scheme: {scheme}"));
    }
    let Some(host) = parsed.host_str() else {
        return Err("URL has no host".into());
    };
    if is_blocked_host(host) {
        return Err("URL targets a private or internal address".into());
    }
    Ok(())
}

/// Validate the URL and every address it resolves to. Port of JS
/// `assertPublicUrlResolved` (blocking `tokio::net::lookup_host`, since the
/// callers are async).
pub async fn assert_public_url_resolved(raw_url: &str) -> Result<(), String> {
    assert_public_url(raw_url)?;
    let parsed = url::Url::parse(raw_url).map_err(|_| "Invalid URL format".to_string())?;
    let Some(host) = parsed.host_str() else {
        return Err("URL has no host".into());
    };
    let Some(port) = parsed.port_or_known_default() else {
        return Ok(());
    };
    // An IP literal was already checked by assert_public_url.
    if host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>().is_ok() {
        return Ok(());
    }
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("DNS lookup failed: {e}"))?;
    for addr in addrs {
        let blocked = match addr.ip() {
            IpAddr::V4(v4) => is_blocked_ipv4(v4.octets()),
            IpAddr::V6(v6) => is_blocked_ipv6(v6),
        };
        if blocked {
            return Err("URL resolves to a private or internal address".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_localhost_and_suffixes() {
        assert!(is_blocked_host("localhost"));
        assert!(is_blocked_host("foo.local"));
        assert!(is_blocked_host("svc.internal"));
        assert!(is_blocked_host("app.localhost"));
    }

    #[test]
    fn blocks_private_ipv4() {
        for h in ["127.0.0.1", "10.1.2.3", "192.168.1.1", "172.16.5.4", "169.254.169.254", "100.64.0.1", "0.0.0.0"] {
            assert!(is_blocked_host(h), "{h} should be blocked");
        }
    }

    #[test]
    fn allows_public_ipv4() {
        for h in ["8.8.8.8", "1.1.1.1", "203.0.113.7"] {
            assert!(!is_blocked_host(h), "{h} should be allowed");
        }
    }

    #[test]
    fn blocks_ipv6_loopback_and_mapped() {
        assert!(is_blocked_host("[::1]"));
        assert!(is_blocked_host("::1"));
        assert!(is_blocked_host("[::ffff:127.0.0.1]"));
        assert!(is_blocked_host("[fe80::1]"));
        assert!(is_blocked_host("[fc00::1]"));
        assert!(!is_blocked_host("[2001:4860:4860::8888]"));
    }

    #[test]
    fn assert_public_url_rejects_metadata_and_schemes() {
        assert!(assert_public_url("http://169.254.169.254/latest/meta-data/").is_err());
        assert!(assert_public_url("file:///etc/passwd").is_err());
        assert!(assert_public_url("https://example.com/page").is_ok());
        assert!(assert_public_url("not a url").is_err());
    }
}
