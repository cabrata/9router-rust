//! Account fallback / cooldown rules. Port of open-sse/services/accountFallback.js
//! and open-sse/config/errorConfig.js.
//!
//! Two different questions drive rotation:
//! - `should_fallback` answers "is the CREDENTIAL at fault?" (account rotation).
//! - `request_scoped` answers "is the REQUEST itself what this member rejected?"
//!   (combo rotation must still try the next member).

use chrono::Utc;

/// Exponential backoff config (BACKOFF_CONFIG).
pub const BACKOFF_BASE_MS: i64 = 2000;
pub const BACKOFF_MAX_MS: i64 = 5 * 60 * 1000;
pub const BACKOFF_MAX_LEVEL: i64 = 15;
/// Default cooldown for transient/unknown errors (TRANSIENT_COOLDOWN_MS).
pub const TRANSIENT_COOLDOWN_MS: i64 = 30 * 1000;
/// Hard cap for provider-reported rate limit cooldown.
pub const MAX_RATE_LIMIT_COOLDOWN_MS: i64 = 30 * 60 * 1000;

const COOLDOWN_LONG_MS: i64 = 2 * 60 * 1000;
const COOLDOWN_SHORT_MS: i64 = 5 * 1000;

/// One unified error-classification rule (ERROR_RULES entry).
struct ErrorRule {
    provider: Option<&'static str>,
    text: Option<&'static str>,
    status: Option<u16>,
    cooldown_ms: i64,
    backoff: bool,
}

const fn text_rule(text: &'static str, cooldown_ms: i64) -> ErrorRule {
    ErrorRule { provider: None, text: Some(text), status: None, cooldown_ms, backoff: false }
}

const fn backoff_rule(text: &'static str) -> ErrorRule {
    ErrorRule { provider: None, text: Some(text), status: None, cooldown_ms: 0, backoff: true }
}

const fn status_rule(status: u16, cooldown_ms: i64) -> ErrorRule {
    ErrorRule { provider: None, text: None, status: Some(status), cooldown_ms, backoff: false }
}

/// Unified error classification rules. Checked top-to-bottom: text rules first
/// (by order = priority), then status rules.
static ERROR_RULES: &[ErrorRule] = &[
    ErrorRule {
        provider: Some("codex"),
        text: Some("model is not supported when using codex with a chatgpt account"),
        status: None,
        cooldown_ms: MAX_RATE_LIMIT_COOLDOWN_MS,
        backoff: false,
    },
    ErrorRule {
        provider: Some("Claude Opus 5.5-intl"),
        text: Some("request illegal"),
        status: None,
        cooldown_ms: 0,
        backoff: true,
    },
    ErrorRule {
        provider: Some("Claude Opus 5.5-intl"),
        text: Some("conversation id missing"),
        status: None,
        cooldown_ms: 0,
        backoff: true,
    },
    text_rule("no credentials", COOLDOWN_LONG_MS),
    text_rule("request not allowed", COOLDOWN_SHORT_MS),
    text_rule("improperly formed request", COOLDOWN_LONG_MS),
    backoff_rule("rate limit"),
    backoff_rule("too many requests"),
    backoff_rule("quota exceeded"),
    backoff_rule("insufficient credit"),
    backoff_rule("credit balance is too low"),
    backoff_rule("capacity"),
    backoff_rule("overloaded"),
    status_rule(401, COOLDOWN_LONG_MS),
    status_rule(402, COOLDOWN_LONG_MS),
    status_rule(403, COOLDOWN_LONG_MS),
    status_rule(404, COOLDOWN_LONG_MS),
    ErrorRule { provider: None, text: None, status: Some(429), cooldown_ms: 0, backoff: true },
];

/// Exponential backoff cooldown for rate limits (429).
/// Level 1: 1s, Level 2: 2s, Level 3: 4s ... capped at 4 min.
pub fn get_quota_cooldown(backoff_level: i64) -> i64 {
    let level = backoff_level.saturating_sub(1).max(0);
    let cooldown = BACKOFF_BASE_MS * 2i64.saturating_pow(level as u32);
    cooldown.min(BACKOFF_MAX_MS)
}

/// Result of [`check_fallback_error`].
#[derive(Clone, Debug)]
pub struct FallbackDecision {
    /// Is the credential at fault? (account rotation)
    pub should_fallback: bool,
    pub cooldown_ms: i64,
    /// New exponential backoff level when the rule uses backoff.
    pub new_backoff_level: Option<i64>,
    /// Is the request itself what this member rejected? (combo rotation)
    pub request_scoped: bool,
}

/// Check if an error should trigger account fallback (switch to next account).
/// Config-driven: matches ERROR_RULES top-to-bottom (text rules first, then status).
pub fn check_fallback_error(status: u16, error_text: &str, backoff_level: i64, provider: Option<&str>) -> FallbackDecision {
    let lower = error_text.to_lowercase();

    for rule in ERROR_RULES {
        if let Some(p) = rule.provider {
            if provider != Some(p) {
                continue;
            }
        }
        if let Some(text) = rule.text {
            if !lower.is_empty() && lower.contains(text) {
                if rule.backoff {
                    let new_level = (backoff_level + 1).min(BACKOFF_MAX_LEVEL);
                    return FallbackDecision {
                        should_fallback: true,
                        cooldown_ms: get_quota_cooldown(new_level),
                        new_backoff_level: Some(new_level),
                        request_scoped: false,
                    };
                }
                return FallbackDecision {
                    should_fallback: true,
                    cooldown_ms: rule.cooldown_ms,
                    new_backoff_level: None,
                    request_scoped: false,
                };
            }
        }
        if let Some(s) = rule.status {
            if s == status {
                if rule.backoff {
                    let new_level = (backoff_level + 1).min(BACKOFF_MAX_LEVEL);
                    return FallbackDecision {
                        should_fallback: true,
                        cooldown_ms: get_quota_cooldown(new_level),
                        new_backoff_level: Some(new_level),
                        request_scoped: false,
                    };
                }
                return FallbackDecision {
                    should_fallback: true,
                    cooldown_ms: rule.cooldown_ms,
                    new_backoff_level: None,
                    request_scoped: false,
                };
            }
        }
    }

    // Request-scoped client errors that matched no rule: a 400 caused by the request
    // itself (context overflow, malformed body, unsupported parameter) says nothing
    // about the credential. Hand the upstream error back for this request instead.
    if (400..500).contains(&status) && status != 401 && status != 402 && status != 403 && status != 429 {
        return FallbackDecision { should_fallback: false, cooldown_ms: 0, new_backoff_level: None, request_scoped: true };
    }

    // Default: transient cooldown for any unmatched error.
    FallbackDecision { should_fallback: true, cooldown_ms: TRANSIENT_COOLDOWN_MS, new_backoff_level: None, request_scoped: false }
}

/// Is a model/account lock on a connection still active?
/// `until` is an RFC3339 timestamp (or null).
pub fn is_lock_active(until: Option<&str>) -> bool {
    let Some(ts) = until else { return false };
    matches!(nr_core::json::parse_time_ms(&serde_json::json!(ts)), Some(t) if t > nr_core::json::now_ms())
}

/// ISO timestamp `cooldown_ms` from now.
pub fn unavailable_until(cooldown_ms: i64) -> String {
    (Utc::now() + chrono::Duration::milliseconds(cooldown_ms)).to_rfc3339()
}

/// Build the flat field key for a per-model lock (`modelLock_<model>`), or
/// `modelLock___all` when no model is known.
pub fn model_lock_key(model: Option<&str>) -> String {
    match model {
        Some(m) if !m.is_empty() => format!("modelLock_{m}"),
        _ => "modelLock___all".to_string(),
    }
}

/// Earliest active `modelLock_*` expiry across all modelLock_* fields (ISO string).
pub fn earliest_model_lock_until(connection: &nr_core::Json) -> Option<String> {
    let obj = connection.as_object()?;
    let now = nr_core::json::now_ms();
    let mut earliest: Option<i64> = None;
    for (key, val) in obj {
        if !key.starts_with("modelLock_") {
            continue;
        }
        let Some(s) = val.as_str() else { continue };
        let Some(t) = nr_core::json::parse_time_ms(&serde_json::json!(s)) else { continue };
        if t <= now {
            continue;
        }
        earliest = Some(earliest.map_or(t, |e: i64| e.min(t)));
    }
    earliest.map(|t| {
        chrono::DateTime::<Utc>::from_timestamp_millis(t)
            .map(|d| d.to_rfc3339())
            .unwrap_or_default()
    })
}

/// Human-readable "reset after Xm Ys" for a rateLimitedUntil ISO timestamp.
pub fn format_retry_after(rate_limited_until: Option<&str>) -> String {
    let Some(ts) = rate_limited_until else { return String::new() };
    let Some(until) = nr_core::json::parse_time_ms(&serde_json::json!(ts)) else {
        return String::new();
    };
    let diff = until - nr_core::json::now_ms();
    if diff <= 0 {
        return "reset after 0s".to_string();
    }
    let total = (diff + 999) / 1000;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    let mut parts: Vec<String> = Vec::new();
    if h > 0 {
        parts.push(format!("{h}h"));
    }
    if m > 0 {
        parts.push(format!("{m}m"));
    }
    if s > 0 || parts.is_empty() {
        parts.push(format!("{s}s"));
    }
    format!("reset after {}", parts.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_cooldown_doubles_and_caps() {
        assert_eq!(get_quota_cooldown(1), 2000);
        assert_eq!(get_quota_cooldown(2), 4000);
        assert_eq!(get_quota_cooldown(0), 2000);
        assert_eq!(get_quota_cooldown(20), BACKOFF_MAX_MS);
    }

    #[test]
    fn rate_limit_text_uses_backoff() {
        let d = check_fallback_error(500, "Rate limit exceeded", 0, None);
        assert!(d.should_fallback);
        assert_eq!(d.new_backoff_level, Some(1));
        assert_eq!(d.cooldown_ms, get_quota_cooldown(1));
    }

    #[test]
    fn status_429_backs_off() {
        let d = check_fallback_error(429, "slow down", 3, None);
        assert!(d.should_fallback);
        assert_eq!(d.new_backoff_level, Some(4));
    }

    #[test]
    fn provider_scoped_rule_only_matches_that_provider() {
        let d = check_fallback_error(400, "request illegal", 0, Some("openai"));
        // No rule matched and 400 is request-scoped.
        assert!(!d.should_fallback);
        assert!(d.request_scoped);

        let d2 = check_fallback_error(400, "request illegal", 0, Some("codebuddy-intl"));
        assert!(d2.should_fallback);
        assert!(d2.new_backoff_level.is_some());
    }

    #[test]
    fn client_400_is_request_scoped_not_account_fault() {
        let d = check_fallback_error(400, "context length exceeded", 0, None);
        assert!(!d.should_fallback);
        assert!(d.request_scoped);
    }

    #[test]
    fn unmatched_500_defaults_to_transient() {
        let d = check_fallback_error(500, "kaboom", 0, None);
        assert!(d.should_fallback);
        assert_eq!(d.cooldown_ms, TRANSIENT_COOLDOWN_MS);
    }

    #[test]
    fn model_lock_key_shape() {
        assert_eq!(model_lock_key(Some("gpt-4")), "modelLock_gpt-4");
        assert_eq!(model_lock_key(None), "modelLock___all");
    }

    #[test]
    fn lock_active_checks_future_timestamp() {
        let future = unavailable_until(60_000);
        assert!(is_lock_active(Some(&future)));
        assert!(!is_lock_active(Some("2000-01-01T00:00:00Z")));
        assert!(!is_lock_active(None));
    }
}
