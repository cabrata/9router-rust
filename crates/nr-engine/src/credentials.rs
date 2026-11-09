//! Provider credential selection and rotation state. Port of src/sse/services/auth.js
//! (`getProviderCredentials`, `markAccountUnavailable`, `clearAccountError`).
//!
//! Connections come from nr-db `providerConnections`, sorted by priority. A
//! connection is unavailable when it is excluded (this request already tried it),
//! when a per-model lock `modelLock_<model>` is in the future, or when the
//! account-level `rateLimitedUntil` is in the future.

use std::collections::HashSet;

use nr_core::json;
use nr_core::{Credentials, Json};
use nr_db::repos::{get_provider_connections, update_provider_connection};
use nr_db::Db;

use crate::account_fallback::{
    check_fallback_error, format_retry_after, is_lock_active, model_lock_key, unavailable_until,
};

/// Per-provider account selection strategy (settings.fallbackStrategy).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Strategy {
    /// Use the highest-priority available connection (default).
    FillFirst,
    /// Sticky round-robin: stay on the most-recently-used until a limit, then rotate.
    RoundRobin,
}

impl Strategy {
    fn from_str(s: &str) -> Self {
        match s {
            "round-robin" => Strategy::RoundRobin,
            _ => Strategy::FillFirst,
        }
    }
}

/// Outcome of a credential lookup.
pub enum CredentialLookup {
    /// A connection was selected.
    Selected(Box<SelectedCredential>),
    /// No connection at all for this provider.
    None,
    /// Connections exist but all are rate-limited / model-locked.
    AllRateLimited {
        retry_after: Option<String>,
        retry_after_human: String,
        last_error: Option<String>,
    },
}

/// One selected provider credential (a connection row flattened into the JS
/// `credentials` shape executors read: `accessToken`, `apiKey`,
/// `providerSpecificData`, `connectionId`, `connectionName`, …).
pub struct SelectedCredential {
    pub connection_id: String,
    pub connection_name: String,
    pub provider: String,
    pub model: Option<String>,
    pub credentials: Credentials,
}

impl SelectedCredential {
    pub fn name_for_log(&self) -> &str {
        if self.connection_name.is_empty() {
            &self.connection_id
        } else {
            &self.connection_name
        }
    }
}

fn conn_display_name(conn: &Json) -> String {
    conn.get("name")
        .and_then(Json::as_str)
        .or_else(|| conn.get("email").and_then(Json::as_str))
        .unwrap_or("")
        .to_string()
}

fn conn_last_error(conn: &Json) -> Option<String> {
    conn.get("lastError").and_then(Json::as_str).map(str::to_string)
}

/// Is a connection excluded by per-model / account-level locks?
fn is_locked_for_model(conn: &Json, model: Option<&str>) -> bool {
    if let Some(m) = model {
        if is_lock_active(conn.get(&model_lock_key(Some(m))).and_then(Json::as_str)) {
            return true;
        }
    }
    is_lock_active(conn.get(&model_lock_key(None)).and_then(Json::as_str))
}

fn is_rate_limited(conn: &Json) -> bool {
    is_lock_active(conn.get("rateLimitedUntil").and_then(Json::as_str))
}

/// Select provider credentials, honoring exclusions and model locks.
///
/// Mirrors JS `getProviderCredentials(provider, excludeConnectionIds, model)`:
/// fill-first by default, round-robin when configured.
pub fn get_provider_credentials(
    db: &Db,
    provider: &str,
    exclude: &HashSet<String>,
    model: Option<&str>,
    strategy: Strategy,
) -> CredentialLookup {
    let filter = json!({ "provider": provider, "isActive": true });
    let Ok(connections) = get_provider_connections(db, Some(&filter)) else {
        return CredentialLookup::None;
    };
    if connections.is_empty() {
        return CredentialLookup::None;
    }

    // Available = not excluded, not model-locked, not rate-limited.
    let available: Vec<&Json> = connections
        .iter()
        .filter(|c| {
            let Some(id) = c.get("id").and_then(Json::as_str) else { return false };
            !exclude.contains(id) && !is_locked_for_model(c, model) && !is_rate_limited(c)
        })
        .collect();

    if available.is_empty() {
        // Find the earliest active lock to report a retry-after.
        let mut earliest: Option<i64> = None;
        let mut last_error: Option<String> = None;
        let now = nr_core::json::now_ms();
        for c in &connections {
            let mut candidates: Vec<Option<&str>> = vec![
                c.get("rateLimitedUntil").and_then(Json::as_str),
                c.get(&model_lock_key(model)).and_then(Json::as_str),
                c.get(&model_lock_key(None)).and_then(Json::as_str),
            ];
            if model.is_none() {
                candidates.push(c.get(&model_lock_key(None)).and_then(Json::as_str));
            }
            for ts in candidates.into_iter().flatten() {
                if let Some(t) = nr_core::json::parse_time_ms(&json!(ts)) {
                    if t > now {
                        earliest = Some(earliest.map_or(t, |e: i64| e.min(t)));
                    }
                }
            }
            if last_error.is_none() {
                last_error = conn_last_error(c);
            }
        }
        let retry_after = earliest.and_then(|t| {
            chrono::DateTime::from_timestamp_millis(t).map(|d| d.to_rfc3339())
        });
        let retry_after_human = format_retry_after(retry_after.as_deref());
        return CredentialLookup::AllRateLimited { retry_after, retry_after_human, last_error };
    }

    let selected = match strategy {
        Strategy::FillFirst => available[0].clone(),
        Strategy::RoundRobin => {
            // Least-recently-used wins; ties fall back to priority order.
            available
                .into_iter()
                .min_by_key(|c| {
                    c.get("lastUsedAt")
                        .and_then(Json::as_str)
                        .and_then(|s| nr_core::json::parse_time_ms(&json!(s)))
                        .unwrap_or(i64::MIN)
                })
                .cloned()
                .expect("available non-empty")
        }
    };

    CredentialLookup::Selected(Box::new(build_selected(provider, model, selected)))
}

fn build_selected(provider: &str, model: Option<&str>, conn: Json) -> SelectedCredential {
    let connection_id = conn.get("id").and_then(Json::as_str).unwrap_or("").to_string();
    let connection_name = conn_display_name(&conn);
    let mut creds = Credentials::from_value(conn.clone());
    // Flat fields executors/chatCore read off credentials.
    creds.set("connectionId", connection_id.clone());
    creds.set("connectionName", connection_name.clone());
    if let Some(m) = model {
        creds.set("model", m.to_string());
    }
    SelectedCredential {
        connection_id,
        connection_name,
        provider: provider.to_string(),
        model: model.map(str::to_string),
        credentials: creds,
    }
}

/// Record that a connection served a request successfully: clear cooldown,
/// model lock and last error (JS `clearAccountError`).
pub fn clear_account_error(db: &Db, connection_id: &str, model: Option<&str>) {
    let mut patch = serde_json::Map::new();
    patch.insert("rateLimitedUntil".into(), Json::Null);
    patch.insert("lastError".into(), Json::Null);
    patch.insert("backoffLevel".into(), json!(0));
    patch.insert(
        model_lock_key(model),
        Json::Null,
    );
    let _ = update_provider_connection(db, connection_id, &Json::Object(patch));
}

/// Record that a connection failed: set cooldown + model lock when the
/// credential is at fault (JS `markAccountUnavailable`).
///
/// Returns the [`FallbackDecision`] so the caller knows whether to rotate.
pub fn mark_account_unavailable(
    db: &Db,
    connection_id: &str,
    status: u16,
    error_text: &str,
    provider: Option<&str>,
    model: Option<&str>,
    backoff_level: i64,
) -> crate::account_fallback::FallbackDecision {
    let decision = check_fallback_error(status, error_text, backoff_level, provider);
    if !decision.should_fallback || decision.cooldown_ms <= 0 {
        return decision;
    }
    let mut patch = serde_json::Map::new();
    patch.insert("rateLimitedUntil".into(), json!(unavailable_until(decision.cooldown_ms)));
    patch.insert(
        "lastError".into(),
        json!(format!("[{status}]: {error_text}")),
    );
    if let Some(level) = decision.new_backoff_level {
        patch.insert("backoffLevel".into(), json!(level));
    }
    // Per-model lock so other models on the same account keep working.
    patch.insert(model_lock_key(model), json!(unavailable_until(decision.cooldown_ms)));
    let _ = update_provider_connection(db, connection_id, &Json::Object(patch));
    decision
}

/// Persist refreshed credentials back onto the connection (JS `updateProviderCredentials`).
pub fn persist_refreshed_credentials(db: &Db, connection_id: &str, refreshed: &Credentials) {
    let _ = update_provider_connection(db, connection_id, &refreshed.clone().into_value());
}

/// Best-effort settings-driven strategy lookup.
pub fn strategy_for_provider(settings: &Json, provider: &str) -> Strategy {
    let provider_override = settings
        .get("providerStrategies")
        .and_then(|s| s.get(provider))
        .and_then(|s| s.get("fallbackStrategy"))
        .and_then(Json::as_str);
    let global = settings.get("fallbackStrategy").and_then(Json::as_str);
    Strategy::from_str(provider_override.or(global).unwrap_or("fill-first"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nr_db::Db;
    use tempfile::TempDir;

    fn setup_db() -> (TempDir, Db) {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        (dir, db)
    }

    fn add_conn(db: &Db, id: &str, provider: &str, priority: i32, api_key: &str) {
        let conn = json!({
            "id": id,
            "provider": provider,
            "authType": "apikey",
            "name": id,
            "priority": priority,
            "isActive": true,
            "providerSpecificData": { "apiKey": api_key }
        });
        nr_db::repos::create_provider_connection(db, &conn).unwrap();
    }

    #[test]
    fn no_credentials_when_provider_has_no_connections() {
        let (_dir, db) = setup_db();
        let exclude = HashSet::new();
        match get_provider_credentials(&db, "openai", &exclude, None, Strategy::FillFirst) {
            CredentialLookup::None => {}
            _ => panic!("expected None"),
        }
    }

    #[test]
    fn fill_first_picks_highest_priority() {
        let (_dir, db) = setup_db();
        add_conn(&db, "c2", "openai", 2, "key2");
        add_conn(&db, "c1", "openai", 1, "key1");
        let exclude = HashSet::new();
        match get_provider_credentials(&db, "openai", &exclude, None, Strategy::FillFirst) {
            CredentialLookup::Selected(s) => {
                assert_eq!(s.connection_id, "c1");
                assert_eq!(s.credentials.api_key(), Some("key1"));
            }
            _ => panic!("expected Selected"),
        }
    }

    #[test]
    fn excluded_connection_is_skipped() {
        let (_dir, db) = setup_db();
        add_conn(&db, "c1", "openai", 1, "key1");
        add_conn(&db, "c2", "openai", 2, "key2");
        let mut exclude = HashSet::new();
        exclude.insert("c1".to_string());
        match get_provider_credentials(&db, "openai", &exclude, None, Strategy::FillFirst) {
            CredentialLookup::Selected(s) => assert_eq!(s.connection_id, "c2"),
            _ => panic!("expected Selected"),
        }
    }

    #[test]
    fn all_rate_limited_when_only_connection_is_locked() {
        let (_dir, db) = setup_db();
        add_conn(&db, "c1", "openai", 1, "key1");
        mark_account_unavailable(&db, "c1", 429, "rate limit", Some("openai"), Some("gpt-4"), 0);
        let exclude = HashSet::new();
        match get_provider_credentials(&db, "openai", &exclude, Some("gpt-4"), Strategy::FillFirst) {
            CredentialLookup::AllRateLimited { retry_after_human, .. } => {
                assert!(retry_after_human.starts_with("reset after"));
            }
            _ => panic!("expected AllRateLimited"),
        }
    }

    #[test]
    fn clear_account_error_releases_lock() {
        let (_dir, db) = setup_db();
        add_conn(&db, "c1", "openai", 1, "key1");
        mark_account_unavailable(&db, "c1", 429, "rate limit", Some("openai"), Some("gpt-4"), 0);
        clear_account_error(&db, "c1", Some("gpt-4"));
        let exclude = HashSet::new();
        match get_provider_credentials(&db, "openai", &exclude, Some("gpt-4"), Strategy::FillFirst) {
            CredentialLookup::Selected(s) => assert_eq!(s.connection_id, "c1"),
            _ => panic!("expected Selected after clear"),
        }
    }

    #[test]
    fn strategy_lookup_reads_settings() {
        let settings = json!({
            "fallbackStrategy": "fill-first",
            "providerStrategies": { "openai": { "fallbackStrategy": "round-robin" } }
        });
        assert_eq!(strategy_for_provider(&settings, "openai"), Strategy::RoundRobin);
        assert_eq!(strategy_for_provider(&settings, "anthropic"), Strategy::FillFirst);
    }
}
