use crate::connection::Db;
use nr_core::Result;
use serde_json::{json, Value};

/// Insert a request log entry
pub fn insert_request_log(db: &Db, log: &Value) -> Result<()> {
    // Extract fields from log object
    let timestamp = log.get("timestamp").and_then(|v| v.as_str()).unwrap_or("");
    let api_key_id = log.get("apiKeyId").and_then(|v| v.as_str());
    let api_key_name = log.get("apiKeyName").and_then(|v| v.as_str());
    let api_key_masked = log.get("apiKeyMasked").and_then(|v| v.as_str());
    let ip = log.get("ip").and_then(|v| v.as_str());
    let method = log.get("method").and_then(|v| v.as_str());
    let path = log.get("path").and_then(|v| v.as_str());
    let endpoint_kind = log.get("endpointKind").and_then(|v| v.as_str());
    let model = log.get("model").and_then(|v| v.as_str());
    let provider = log.get("provider").and_then(|v| v.as_str());
    let resolved_model = log.get("resolvedModel").and_then(|v| v.as_str());
    let status = log.get("status").and_then(|v| v.as_i64()).unwrap_or(0);
    let stream = log.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let prompt_tokens = log.get("promptTokens").and_then(|v| v.as_i64()).unwrap_or(0);
    let completion_tokens = log.get("completionTokens").and_then(|v| v.as_i64()).unwrap_or(0);
    let duration_ms = log.get("durationMs").and_then(|v| v.as_i64()).unwrap_or(0);
    let ttft_ms = log.get("ttftMs").and_then(|v| v.as_i64()).unwrap_or(0);
    let tps = log.get("tps").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let user_agent = log.get("userAgent").and_then(|v| v.as_str());
    let error = log.get("error").and_then(|v| v.as_str());

    // TODO(dedupe): Implement log pruning logic (max rows, prune every N inserts)
    db.execute_with_params(
        "INSERT INTO requestLogs(timestamp, apiKeyId, apiKeyName, apiKeyMasked, ip, method, path, endpointKind, model, provider, resolvedModel, status, stream, promptTokens, completionTokens, durationMs, ttftMs, tps, userAgent, error) VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        [
            timestamp.to_string(),
            api_key_id.unwrap_or("").to_string(),
            api_key_name.unwrap_or("").to_string(),
            api_key_masked.unwrap_or("").to_string(),
            ip.unwrap_or("").to_string(),
            method.unwrap_or("").to_string(),
            path.unwrap_or("").to_string(),
            endpoint_kind.unwrap_or("").to_string(),
            model.unwrap_or("").to_string(),
            provider.unwrap_or("").to_string(),
            resolved_model.unwrap_or("").to_string(),
            status.to_string(),
            (if stream { 1 } else { 0 }).to_string(),
            prompt_tokens.to_string(),
            completion_tokens.to_string(),
            duration_ms.to_string(),
            ttft_ms.to_string(),
            tps.to_string(),
            user_agent.unwrap_or("").to_string(),
            error.unwrap_or("").to_string(),
        ],
    )?;

    Ok(())
}

/// Get recent request logs
pub fn get_recent_logs(db: &Db, limit: usize) -> Result<Vec<Value>> {
    db.query_all(
        &format!(
            "SELECT timestamp, apiKeyId, apiKeyName, model, status, durationMs, error FROM requestLogs ORDER BY timestamp DESC LIMIT {}",
            limit
        ),
        &[],
        |row| {
            Ok(json!({
                "timestamp": row.get::<_, String>(0)?,
                "apiKeyId": row.get::<_, Option<String>>(1)?,
                "apiKeyName": row.get::<_, Option<String>>(2)?,
                "model": row.get::<_, Option<String>>(3)?,
                "status": row.get::<_, Option<i32>>(4)?,
                "durationMs": row.get::<_, Option<i32>>(5)?,
                "error": row.get::<_, Option<String>>(6)?,
            }))
        },
    )
}

/// Get API key monitoring stats
pub fn get_api_key_monitoring(db: &Db, key_id: &str) -> Result<Value> {
    let since_24h = chrono::Utc::now()
        .checked_sub_signed(chrono::Duration::hours(24))
        .unwrap_or_else(|| chrono::Utc::now())
        .to_rfc3339();

    let stats = db.query_row(
        "SELECT COUNT(*) as requests_24h, SUM(CASE WHEN status >= 400 THEN 1 ELSE 0 END) as errors_24h, MAX(timestamp) as last_request FROM requestLogs WHERE apiKeyId = ? AND timestamp >= ?",
        &[&key_id, &since_24h],
        |row| {
            let requests: i32 = row.get(0).unwrap_or(0);
            let errors: i32 = row.get(1).unwrap_or(0);
            let last_request: Option<String> = row.get(2).ok().flatten();
            Ok((requests, errors, last_request))
        },
    )?;

    match stats {
        Some((requests, errors, last_request)) => {
            Ok(json!({
                "activeRequests": 0,
                "queuedRequests": 0,
                "requestsLastMinute": 0,
                "requestsLast24h": requests,
                "errorsLast24h": errors,
                "lastRequestAt": last_request,
            }))
        }
        None => {
            Ok(json!({
                "activeRequests": 0,
                "queuedRequests": 0,
                "requestsLastMinute": 0,
                "requestsLast24h": 0,
                "errorsLast24h": 0,
                "lastRequestAt": Value::Null,
            }))
        }
    }
}

/// Acquire an API key request slot (returns lease info)
pub fn acquire_api_key_request(_db: &Db, _key: &Value) -> Result<Value> {
    // TODO(dedupe): Implement full request rate limiting with queue/slot management
    Ok(json!({
        "ok": true,
        "release": Value::Null,
    }))
}

/// Async local storage for request context
pub struct RequestLogStore;

impl RequestLogStore {
    pub async fn run<F, T>(_ctx: Value, f: F) -> Result<T>
    where
        F: std::future::Future<Output = T>,
    {
        Ok(f.await)
    }
}

pub static REQUEST_LOG_STORE: RequestLogStore = RequestLogStore;

pub fn request_log_store() -> &'static RequestLogStore {
    &REQUEST_LOG_STORE
}
