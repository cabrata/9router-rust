//! Port of open-sse/services/oauthCredentialManager.js.
//!
//! FROZEN signature below (executors call it).

use chrono::Utc;
use serde_json::json;

use nr_core::{Credentials, Json, JsonMap, Logger};
use crate::token_refresh::{get_refresh_lead_ms, is_unrecoverable_refresh_error};

/// Helper: parse a time field from credentials (handles ms, s, ISO string).
fn parse_time_ms(value: Option<&Json>) -> Option<i64> {
    match value {
        None => None,
        Some(Json::Number(n)) => {
            let ms = n.as_i64()?;
            // If < 1e12, treat as seconds; else as milliseconds
            if ms < 1_000_000_000_000 {
                Some(ms * 1000)
            } else {
                Some(ms)
            }
        }
        Some(Json::String(s)) => {
            // Parse ISO 8601 date string
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
                Some(dt.timestamp_millis())
            } else {
                None
            }
        }
        _ => None,
    }
}

pub fn get_credential_expiry_ms(creds: &Credentials) -> Option<i64> {
    parse_time_ms(creds.get("expiresAt")).or_else(|| parse_time_ms(creds.get("tokenExpiresAt")))
}

pub fn get_credential_last_refresh_ms(creds: &Credentials) -> Option<i64> {
    parse_time_ms(creds.get("lastRefreshAt"))
        .or_else(|| parse_time_ms(creds.get("lastRefresh")))
        .or_else(|| {
            creds.psd()?.get("lastRefreshAt").and_then(|v| parse_time_ms(Some(v)))
        })
}

/// JS `shouldRefreshCredentials(provider, credentials, nowMs = Date.now())`.
pub fn should_refresh_credentials(provider: &str, creds: &Credentials, now_ms: i64) -> bool {
    if creds.0.is_empty() {
        return false;
    }

    // Check if expiry is approaching
    if let Some(expires_at_ms) = get_credential_expiry_ms(creds) {
        let lead_ms = get_refresh_lead_ms(provider);
        if expires_at_ms - now_ms < lead_ms {
            return true;
        }
    }

    // TODO(wave2): proactive stale refresh for providers with maxRefreshAgeMs (e.g. codex)
    // if maxAgeMs is set and credentials.refreshToken exists and token is stale by age

    false
}

pub fn merge_provider_specific_data(existing: Option<&JsonMap>, next: Option<&JsonMap>) -> Option<JsonMap> {
    match (existing, next) {
        (_, None) => existing.cloned(),
        (None, Some(n)) => Some(n.clone()),
        (Some(e), Some(n)) => {
            let mut result = e.clone();
            for (k, v) in n {
                result.insert(k.clone(), v.clone());
            }
            Some(result)
        }
    }
}

pub fn merge_refreshed_credentials(
    _provider: &str,
    current_creds: &Credentials,
    refreshed: &Option<JsonMap>,
    now_ms: i64,
) -> Option<JsonMap> {
    let refreshed = refreshed.as_ref()?;

    // Fail if this is an unrecoverable error
    if is_unrecoverable_refresh_error(&Some(refreshed.clone())) {
        return Some(refreshed.clone());
    }

    let mut next = JsonMap::new();
    // Convert now_ms back to UTC timestamp for ISO string
    let now_iso = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now_ms)
        .unwrap_or_else(|| Utc::now())
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

    // Copy access token, API key, or token field
    if let Some(at) = refreshed.get("accessToken") {
        next.insert("accessToken".to_string(), at.clone());
    }
    if let Some(ak) = refreshed.get("apiKey") {
        next.insert("apiKey".to_string(), ak.clone());
    }
    if let Some(t) = refreshed.get("token") {
        next.insert("token".to_string(), t.clone());
    }

    // Refresh token (keep old if not provided)
    let refresh_token = refreshed
        .get("refreshToken")
        .or_else(|| current_creds.get("refreshToken"));
    if let Some(rt) = refresh_token {
        next.insert("refreshToken".to_string(), rt.clone());
    }

    // ID token
    let id_token = refreshed
        .get("idToken")
        .or_else(|| current_creds.get("idToken"));
    if let Some(it) = id_token {
        next.insert("idToken".to_string(), it.clone());
    }

    // Expiry handling
    if let Some(expires_in) = refreshed.get("expiresIn") {
        next.insert("expiresIn".to_string(), expires_in.clone());
        // Calculate expiresAt as ISO string
        if let Some(seconds) = expires_in.as_i64() {
            let expires_at = Utc::now()
                .checked_add_signed(chrono::Duration::milliseconds(seconds * 1000))
                .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
            if let Some(exp_str) = expires_at {
                next.insert("expiresAt".to_string(), json!(exp_str));
            }
        }
    } else if let Some(ea) = refreshed.get("expiresAt") {
        next.insert("expiresAt".to_string(), ea.clone());
    }

    // Project ID
    if let Some(pid) = refreshed.get("projectId") {
        next.insert("projectId".to_string(), pid.clone());
    }

    // Provider-specific data
    if let Some(psd) = refreshed.get("providerSpecificData").and_then(|v| v.as_object()) {
        if let Some(merged) = merge_provider_specific_data(current_creds.psd(), Some(psd)) {
            next.insert(
                "providerSpecificData".to_string(),
                Json::Object(merged),
            );
        }
    }

    // Copilot token (GitHub Copilot)
    if let Some(ct) = refreshed.get("copilotToken") {
        next.insert("copilotToken".to_string(), ct.clone());
    }
    if let Some(cte) = refreshed.get("copilotTokenExpiresAt") {
        next.insert("copilotTokenExpiresAt".to_string(), cte.clone());
    }

    // Track lastRefreshAt for staleness detection
    // TODO(wave2): read trackRefreshAt from provider config
    let should_track_refresh = next.get("accessToken").is_some()
        || next.get("apiKey").is_some()
        || next.get("token").is_some()
        || next.get("refreshToken").is_some()
        || next.get("copilotToken").is_some();

    if should_track_refresh {
        next.insert(
            "lastRefreshAt".to_string(),
            refreshed
                .get("lastRefreshAt")
                .cloned()
                .unwrap_or_else(|| json!(now_iso)),
        );
    }

    Some(next)
}

pub async fn with_credential_refresh_lock<F, Fut, T>(
    _provider: &str,
    _creds: &Credentials,
    f: F,
) -> Option<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    // TODO(wave2): implement actual lock mechanism to prevent concurrent refreshes
    // For now, just call the function directly
    f().await
}

pub async fn refresh_provider_credentials(
    _provider: &str,
    _creds: &Credentials,
    log: &Logger,
) -> Option<JsonMap> {
    // TODO(wave2): dispatch to refresh_token_by_provider once available
    // For now, return None
    log.warn("TOKEN_REFRESH", "Credential refresh not yet implemented");
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_time_ms() {
        // Milliseconds
        let ms_val = json!(1700000000000i64);
        assert_eq!(parse_time_ms(Some(&ms_val)), Some(1700000000000));

        // Seconds (< 1e12)
        let sec_val = json!(1700000000i64);
        assert_eq!(parse_time_ms(Some(&sec_val)), Some(1700000000000));

        // ISO string
        let iso_val = json!("2024-01-01T00:00:00Z");
        assert!(parse_time_ms(Some(&iso_val)).is_some());

        // None
        assert_eq!(parse_time_ms(None), None);
    }

    #[test]
    fn test_should_refresh_credentials() {
        let mut creds = Credentials::new();
        let now_ms = Utc::now().timestamp_millis();

        // No credentials -> no refresh
        assert!(!should_refresh_credentials("test", &creds, now_ms));

        // Token expiring soon
        let expiry = Utc::now()
            .checked_add_signed(chrono::Duration::minutes(2))
            .unwrap()
            .timestamp_millis();
        creds.set("expiresAt", json!(expiry));
        assert!(should_refresh_credentials("test", &creds, now_ms));

        // Token expiring far in the future
        let far_future = Utc::now()
            .checked_add_signed(chrono::Duration::hours(10))
            .unwrap()
            .timestamp_millis();
        creds.set("expiresAt", json!(far_future));
        assert!(!should_refresh_credentials("test", &creds, now_ms));
    }
}
