//! Port of `open-sse/services/projectId.js`: fetch and cache the real Project
//! ID from Google Cloud Code API for a connection, instead of generating a
//! random id. `getProjectIdForConnection` dedupes concurrent fetches per
//! connection and stores the result with a 1 h TTL.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nr_core::http::{FetchRequest, HeaderList, proxy_fetch};
use nr_core::Json;
use serde_json::json;

/// `CACHE_TTL_MS`: how long a cached project id counts as fresh (1 h).
const CACHE_TTL: Duration = Duration::from_secs(60 * 60);
/// `PENDING_TTL_MS`: abort and evict a pending fetch older than 2 min.
const PENDING_TTL: Duration = Duration::from_secs(2 * 60);
/// Per-attempt onboard timeout (30 s in JS).
const ONBOARD_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);
/// `BASE_RETRY_DELAY_MS` + jitter between onboardUser polls.
const ONBOARD_RETRY_DELAY: Duration = Duration::from_secs(12);
/// `MAX_ATTEMPTS` for onboardUser (env-overridable).
fn onboard_max_attempts() -> u32 {
    std::env::var("ONBOARD_MAX_ATTEMPTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2)
}

/// `CLOUD_CODE_API`: project discovery stays on the PROD host for every
/// provider — the daily host rejects auth/onboarding calls.
const LOAD_CODE_ASSIST_URL: &str = "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist";
const ONBOARD_USER_URL: &str = "https://cloudcode-pa.googleapis.com/v1internal:onboardUser";

/// `LOAD_CODE_ASSIST_METADATA` shared by loadCodeAssist and onboardUser.
fn code_assist_metadata() -> Json {
    json!({
        "ideType": 9, // IDE_TYPE.ANTIGRAVITY
        "platform": nr_data::get_platform_enum(),
        "pluginType": 2, // PLUGIN_TYPE.GEMINI
    })
}

/// Headers for loadCodeAssist/onboardUser. Antigravity sends no fingerprint
/// headers (the backend silently refuses to provision a project with them).
fn code_assist_headers(provider: &str, access_token: &str) -> HeaderList {
    let mut headers = HeaderList::new();
    headers.insert("Content-Type".into(), "application/json".into());
    headers.insert("Authorization".into(), format!("Bearer {access_token}"));
    if provider != "antigravity" {
        headers.insert("User-Agent".into(), "google-api-nodejs-client/9.15.1".into());
        headers.insert("X-Goog-Api-Client".into(), "google-cloud-sdk vscode_cloudshelleditor/0.1".into());
    }
    headers
}

/// `extractProjectId(data)`: `cloudaicompanionProject` string or `{id}`.
fn extract_project_id(data: &Json) -> Option<String> {
    let project = data.get("cloudaicompanionProject")?;
    match project {
        Json::String(s) => {
            let id = s.trim();
            (!id.is_empty()).then(|| id.to_string())
        }
        Json::Object(_) => project.get("id").and_then(Json::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string),
        _ => None,
    }
}

/// `extractProjectIdFromOnboard(data)`: `response.cloudaicompanionProject`.
fn extract_project_id_from_onboard(data: &Json) -> Option<String> {
    let response = data.get("response")?;
    let project = response.get("cloudaicompanionProject")?;
    match project {
        Json::String(s) => {
            let id = s.trim();
            (!id.is_empty()).then(|| id.to_string())
        }
        Json::Object(_) => project.get("id").and_then(Json::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string),
        _ => None,
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

struct CacheEntry {
    project_id: String,
    fetched_at_ms: u64,
}

struct PendingFetch {
    started_at_ms: u64,
}

/// connectionId -> { projectId, fetchedAt }.
static PROJECT_ID_CACHE: LazyLock<Mutex<HashMap<String, CacheEntry>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
/// connectionId -> pending-fetch marker (dedup + abort flag).
static PENDING: LazyLock<Mutex<HashMap<String, PendingFetch>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// `cleanupNow()`: evict stale cache entries and drop orphaned pending fetches.
pub fn cleanup_now() {
    let now = now_ms();
    PROJECT_ID_CACHE.lock().expect("projectId cache").retain(|_, e| now.saturating_sub(e.fetched_at_ms) < CACHE_TTL.as_millis() as u64);
    PENDING.lock().expect("projectId pending").retain(|_, p| now.saturating_sub(p.started_at_ms) <= PENDING_TTL.as_millis() as u64);
}

/// `invalidateProjectId(connectionId)`.
pub fn invalidate_project_id(connection_id: &str) {
    PROJECT_ID_CACHE.lock().expect("projectId cache").remove(connection_id);
}

/// `removeConnection(connectionId)`: drop the cached id and abort an in-flight
/// fetch. Call from the connection-close lifecycle to avoid memory leaks.
pub fn remove_connection(connection_id: &str) {
    if connection_id.is_empty() {
        return;
    }
    PROJECT_ID_CACHE.lock().expect("projectId cache").remove(connection_id);
    PENDING.lock().expect("projectId pending").remove(connection_id);
}

/// `getProjectIdForConnection(connectionId, accessToken, provider)`, with a
/// 1 h cache and per-connection fetch dedup. `None` on failure (callers fall
/// back to random generation).
pub async fn get_project_id_for_connection(connection_id: &str, access_token: &str, provider: &str) -> Option<String> {
    if connection_id.is_empty() || access_token.is_empty() {
        return None;
    }

    let now = now_ms();
    if let Some(cached) = PROJECT_ID_CACHE.lock().expect("projectId cache").get(connection_id) {
        if now.saturating_sub(cached.fetched_at_ms) < CACHE_TTL.as_millis() as u64 {
            return Some(cached.project_id.clone());
        }
    }

    // Dedup concurrent fetches for the same connection.
    if PENDING.lock().expect("projectId pending").contains_key(connection_id) {
        // The first caller keeps the fetch alive; wait for its outcome by
        // polling the cache (the fetch is in flight under the same key).
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            if let Some(cached) = PROJECT_ID_CACHE.lock().expect("projectId cache").get(connection_id) {
                return Some(cached.project_id.clone());
            }
            if !PENDING.lock().expect("projectId pending").contains_key(connection_id) {
                return None;
            }
        }
        return None;
    }

    PENDING.lock().expect("projectId pending").insert(connection_id.to_string(), PendingFetch { started_at_ms: now });

    let result = fetch_project_id(access_token, provider).await;
    if let Some(project_id) = &result {
        PROJECT_ID_CACHE.lock().expect("projectId cache").insert(
            connection_id.to_string(),
            CacheEntry { project_id: project_id.clone(), fetched_at_ms: now_ms() },
        );
    }
    PENDING.lock().expect("projectId pending").remove(connection_id);
    result
}

/// `fetchProjectId`: loadCodeAssist, then onboardUser when it returns no
/// project. On transport error returns `None` (the JS callers catch and
/// degrade to random ids).
async fn fetch_project_id(access_token: &str, provider: &str) -> Option<String> {
    let mut req = FetchRequest::post_json(
        LOAD_CODE_ASSIST_URL,
        code_assist_headers(provider, access_token),
        &json!({ "metadata": code_assist_metadata() }),
    );
    req.header_timeout = Some(Duration::from_secs(30));
    let response = proxy_fetch(req, None).await.ok()?;
    if !response.ok() {
        return None;
    }
    let data = response.json_body().await.ok()?;
    if let Some(id) = extract_project_id(&data) {
        return Some(id);
    }

    // Default tier, then the first isDefault tier listed.
    let mut tier_id = "legacy-tier".to_string();
    if let Some(tiers) = data.get("allowedTiers").and_then(Json::as_array) {
        for tier in tiers {
            if tier.get("isDefault").and_then(Json::as_bool) == Some(true) {
                if let Some(id) = tier.get("id").and_then(Json::as_str).map(str::trim).filter(|s| !s.is_empty()) {
                    tier_id = id.to_string();
                    break;
                }
            }
        }
    }
    onboard_user(access_token, &tier_id, provider).await
}

/// `onboardUser`: poll until `done`, with a per-attempt timeout and bounded
/// retries with jitter.
async fn onboard_user(access_token: &str, tier_id: &str, provider: &str) -> Option<String> {
    let max_attempts = onboard_max_attempts();
    let body = json!({ "tierId": tier_id, "metadata": code_assist_metadata() });
    for _ in 0..max_attempts {
        let mut req = FetchRequest::post_json(
            ONBOARD_USER_URL,
            code_assist_headers(provider, access_token),
            &body,
        );
        req.header_timeout = Some(ONBOARD_ATTEMPT_TIMEOUT);
        let Ok(response) = proxy_fetch(req, None).await else {
            sleep_with_jitter().await;
            continue;
        };
        if !response.ok() {
            sleep_with_jitter().await;
            continue;
        }
        let Ok(data) = response.json_body().await else {
            return None;
        };
        if data.get("done").and_then(Json::as_bool) == Some(true) {
            return extract_project_id_from_onboard(&data);
        }
        sleep_with_jitter().await;
    }
    None
}

async fn sleep_with_jitter() {
    let jitter = rand::random::<u64>() % 5000;
    tokio::time::sleep(ONBOARD_RETRY_DELAY + Duration::from_millis(jitter)).await;
}

/// `getCachedProjectId(connectionId)` — synchronous read for hot paths.
pub fn get_cached_project_id(connection_id: &str) -> Option<String> {
    let now = now_ms();
    PROJECT_ID_CACHE.lock().expect("projectId cache").get(connection_id).and_then(|e| {
        (now.saturating_sub(e.fetched_at_ms) < CACHE_TTL.as_millis() as u64).then(|| e.project_id.clone())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_project_id_handles_string_and_object() {
        assert_eq!(extract_project_id(&json!({ "cloudaicompanionProject": "  abc-123  " })), Some("abc-123".into()));
        assert_eq!(extract_project_id(&json!({ "cloudaicompanionProject": { "id": "obj-id" } })), Some("obj-id".into()));
        assert_eq!(extract_project_id(&json!({ "cloudaicompanionProject": { "id": "  " } })), None);
        assert_eq!(extract_project_id(&json!({})), None);
        assert_eq!(extract_project_id(&json!({ "cloudaicompanionProject": 5 })), None);
    }

    #[test]
    fn extract_project_id_from_onboard_reads_response() {
        assert_eq!(extract_project_id_from_onboard(&json!({ "response": { "cloudaicompanionProject": "p1" } })), Some("p1".into()));
        assert_eq!(extract_project_id_from_onboard(&json!({ "response": { "cloudaicompanionProject": { "id": "p2" } } })), Some("p2".into()));
        assert_eq!(extract_project_id_from_onboard(&json!({ "response": {} })), None);
        assert_eq!(extract_project_id_from_onboard(&json!({})), None);
    }

    #[test]
    fn cache_ttl_and_invalidation() {
        let id = "conn-ttl";
        PROJECT_ID_CACHE.lock().unwrap().insert(id.into(), CacheEntry { project_id: "p".into(), fetched_at_ms: now_ms() });
        assert_eq!(get_cached_project_id(id), Some("p".into()));
        invalidate_project_id(id);
        assert_eq!(get_cached_project_id(id), None);
    }

    #[test]
    fn remove_connection_clears_cache() {
        let id = "conn-remove";
        PROJECT_ID_CACHE.lock().unwrap().insert(id.into(), CacheEntry { project_id: "p".into(), fetched_at_ms: now_ms() });
        PENDING.lock().unwrap().insert(id.into(), PendingFetch { started_at_ms: now_ms() });
        remove_connection(id);
        assert_eq!(get_cached_project_id(id), None);
        assert!(!PENDING.lock().unwrap().contains_key(id));
    }
}
