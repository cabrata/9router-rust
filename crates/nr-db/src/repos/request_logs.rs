//! Port of src/lib/db/repos/requestLogsRepo.js.
//!
//! `requestLogs` is the per-request audit table (distinct from usageHistory).
//! Rows are pruned down to a fixed cap every PRUNE_EVERY inserts, exactly like
//! the JS module's module-level counters.

use crate::connection::Db;
use crate::helpers::{clean_param, parse_json};
use nr_core::Result;
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// `process.env.REQUEST_LOGS_MAX || "5000"` — fixed row cap.
const MAX_ROWS: i64 = 5000;
/// `PRUNE_EVERY`: prune every N inserts.
const PRUNE_EVERY: usize = 100;

static INSERTS_SINCE_PRUNE: AtomicUsize = AtomicUsize::new(0);

/// Column order must match the JS COLS array: the INSERT is positional and the
/// same SQLite file is shared with the JS build.
const COLS: [&str; 20] = [
    "timestamp", "apiKeyId", "apiKeyName", "apiKeyMasked", "ip", "method", "path", "endpointKind",
    "model", "provider", "resolvedModel", "status", "stream", "promptTokens", "completionTokens",
    "durationMs", "ttftMs", "tps", "userAgent", "error",
];

/// Insert a request log entry, pruning the table every `PRUNE_EVERY` inserts.
pub fn insert_request_log(db: &Db, log: &Value) -> Result<()> {
    // Undefined JS fields bind as NULL; booleans become 0/1. Bind rusqlite's own
    // value type: serde_json::Value's ToSql JSON-encodes strings.
    let mut values: Vec<rusqlite::types::Value> = Vec::with_capacity(COLS.len());
    for col in COLS {
        let v = match log.get(col) {
            None | Some(Value::Null) => rusqlite::types::Value::Null,
            Some(Value::Bool(b)) => rusqlite::types::Value::Integer(i64::from(*b)),
            // SQLite TEXT cannot hold NUL bytes.
            Some(Value::String(s)) => {
                rusqlite::types::Value::Text(clean_param(s.clone()))
            }
            Some(Value::Number(n)) if n.is_i64() => {
                rusqlite::types::Value::Integer(n.as_i64().unwrap_or_default())
            }
            Some(Value::Number(n)) => {
                rusqlite::types::Value::Real(n.as_f64().unwrap_or_default())
            }
            // Objects/arrays are not valid requestLogs columns; store their JSON.
            Some(other) => rusqlite::types::Value::Text(other.to_string()),
        };
        values.push(v);
    }

    let params: Vec<&dyn rusqlite::ToSql> = values
        .iter()
        .map(|v| v as &dyn rusqlite::ToSql)
        .collect();
    db.execute_with_params(
        &format!(
            "INSERT INTO requestLogs({}) VALUES({})",
            COLS.join(", "),
            COLS.iter().map(|_| "?").collect::<Vec<_>>().join(", ")
        ),
        params.as_slice(),
    )?;

    if INSERTS_SINCE_PRUNE.fetch_add(1, Ordering::Relaxed) + 1 >= PRUNE_EVERY {
        INSERTS_SINCE_PRUNE.store(0, Ordering::Relaxed);
        // Keep the newest MAX_ROWS rows; the subquery picks the id at that offset.
        db.execute_with_params(
            "DELETE FROM requestLogs WHERE id <= (SELECT id FROM requestLogs ORDER BY id DESC LIMIT 1 OFFSET ?)",
            [MAX_ROWS],
        )?;
    }

    Ok(())
}

/// Compat provider IDs look like "openai-compatible-chat-<uuid>"; show the
/// user-chosen node name instead, then the alias prefix, then a short id.
fn provider_label(provider: &str, nodes: &Map<String, Value>, aliases: &Map<String, Value>) -> String {
    if provider.is_empty() {
        return provider.to_string();
    }
    if let Some(node_name) = nodes.get(provider).and_then(Value::as_str) {
        return node_name.to_string();
    }
    if let Some(target) = aliases.get(provider).and_then(Value::as_str) {
        if let Some((prefix, _)) = target.split_once('/') {
            return prefix.to_string();
        }
    }
    // /^(openai|anthropic)-compatible-(?:responses-)?[0-9a-f]{8}-/i
    let lowered = provider.to_ascii_lowercase();
    for family in ["openai", "anthropic"] {
        let prefix = format!("{family}-compatible-");
        let Some(rest) = lowered.strip_prefix(&prefix) else {
            continue;
        };
        let rest = rest.strip_prefix("responses-").unwrap_or(rest);
        let tail = rest.split_once('-');
        if let Some((hex_prefix, _)) = tail {
            if hex_prefix.len() == 8 && hex_prefix.chars().all(|c| c.is_ascii_hexdigit()) {
                return format!("{family}-compatible-{}", &provider[provider.len() - 4..]);
            }
        }
    }
    provider.to_string()
}

/// Filter keys accepted by `getRequestLogs`.
#[derive(Default)]
pub struct RequestLogFilter {
    pub api_key_id: Option<String>,
    pub status: Option<String>,
    pub endpoint_kind: Option<String>,
    pub search: Option<String>,
    pub page: Option<i64>,
    pub page_size: Option<i64>,
}

impl RequestLogFilter {
    /// Build from a JSON filter object (route query params).
    pub fn from_json(filter: Option<&Value>) -> Self {
        let f = filter.unwrap_or(&Value::Null);
        let s = |key: &str| {
            f.get(key)
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        RequestLogFilter {
            api_key_id: s("apiKeyId"),
            status: s("status"),
            endpoint_kind: s("endpointKind"),
            search: s("search"),
            page: f.get("page").and_then(Value::as_i64),
            page_size: f.get("pageSize").and_then(Value::as_i64),
        }
    }
}

/// `getRequestLogs()`: filtered, paginated rows with provider label rewriting
/// and `stream` coerced to a boolean.
pub fn get_request_logs(db: &Db, filter: &RequestLogFilter) -> Result<Value> {
    let mut conds: Vec<&str> = Vec::new();
    let mut params: Vec<String> = Vec::new();

    if let Some(api_key_id) = &filter.api_key_id {
        if api_key_id == "none" {
            conds.push("apiKeyId IS NULL");
        } else {
            conds.push("apiKeyId = ?");
            params.push(api_key_id.clone());
        }
    }
    match filter.status.as_deref() {
        Some("ok") => conds.push("status < 400"),
        Some("error") => conds.push("status >= 400"),
        _ => {}
    }
    if let Some(endpoint_kind) = &filter.endpoint_kind {
        conds.push("endpointKind = ?");
        params.push(endpoint_kind.clone());
    }
    if let Some(search) = &filter.search {
        let q = format!("%{search}%");
        conds.push("(model LIKE ? OR resolvedModel LIKE ? OR ip LIKE ? OR path LIKE ? OR provider LIKE ?)");
        for _ in 0..5 {
            params.push(q.clone());
        }
    }

    let where_sql = if conds.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conds.join(" AND "))
    };

    let total: i64 = db
        .query_row(
            &format!("SELECT COUNT(*) as c FROM requestLogs {where_sql}"),
            &params
                .iter()
                .map(|p| p as &dyn rusqlite::ToSql)
                .collect::<Vec<_>>(),
            |row| row.get(0),
        )?
        .unwrap_or(0);

    let page = filter.page.unwrap_or(1).max(1);
    let page_size = filter.page_size.unwrap_or(50).clamp(1, 200);
    let offset = (page - 1) * page_size;

    let mut all_params: Vec<&dyn rusqlite::ToSql> = params
        .iter()
        .map(|p| p as &dyn rusqlite::ToSql)
        .collect();
    all_params.push(&page_size);
    all_params.push(&offset);

    let mut logs = db.query_all(
        &format!(
            "SELECT {} FROM requestLogs {where_sql} ORDER BY id DESC LIMIT ? OFFSET ?",
            LOG_COLS_WITH_ID
        ),
        &all_params,
        row_to_log,
    )?;

    // Only load node names/aliases when a compat provider id is actually present.
    let needs_labels = logs.iter().any(|r| {
        r.get("provider")
            .and_then(Value::as_str)
            .map(|p| !p.contains('/') && p.contains("-compatible-"))
            .unwrap_or(false)
    });
    if needs_labels {
        let nodes: Map<String, Value> = crate::repos::nodes::get_provider_nodes(db, None)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|n| {
                let id = n.get("id").and_then(Value::as_str)?.to_string();
                let name = n.get("name").cloned().unwrap_or(Value::Null);
                Some((id, name))
            })
            .collect();
        let aliases = crate::repos::aliases::get_model_aliases(db)
            .unwrap_or_else(|_| json!({}))
            .as_object()
            .cloned()
            .unwrap_or_default();
        for log in &mut logs {
            if let Some(provider) = log.get("provider").and_then(Value::as_str) {
                let label = provider_label(provider, &nodes, &aliases);
                if let Some(obj) = log.as_object_mut() {
                    obj.insert("provider".into(), Value::String(label));
                }
            }
        }
    }

    let total_pages = if page_size > 0 {
        (total + page_size - 1) / page_size
    } else {
        0
    };

    Ok(json!({
        "logs": logs,
        "pagination": {
            "page": page,
            "pageSize": page_size,
            "totalItems": total,
            "totalPages": total_pages,
        }
    }))
}

/// The `requestLogs` projection used by [`get_request_logs`]. Named columns, not
/// `SELECT *`, so a database whose physical column order differs (an older file
/// grown via ALTER TABLE) still maps by name.
const LOG_COLS_WITH_ID: &str = "id, timestamp, apiKeyId, apiKeyName, apiKeyMasked, ip, method, path, endpointKind, model, provider, resolvedModel, status, stream, promptTokens, completionTokens, durationMs, ttftMs, tps, userAgent, error";

/// Row -> log object for [`LOG_COLS_WITH_ID`], coercing `stream` (stored as
/// 0/1) back to a boolean.
fn row_to_log(row: &rusqlite::Row) -> rusqlite::Result<Value> {
    let text = |idx: usize| -> Option<String> { row.get::<_, Option<String>>(idx).ok().flatten() };
    let int = |idx: usize| -> Option<i64> { row.get::<_, Option<i64>>(idx).ok().flatten() };
    let real = |idx: usize| -> Option<f64> { row.get::<_, Option<f64>>(idx).ok().flatten() };

    let mut obj = Map::new();
    obj.insert("id".into(), json!(int(0)));
    obj.insert("timestamp".into(), json!(text(1)));
    obj.insert("apiKeyId".into(), json!(text(2)));
    obj.insert("apiKeyName".into(), json!(text(3)));
    obj.insert("apiKeyMasked".into(), json!(text(4)));
    obj.insert("ip".into(), json!(text(5)));
    obj.insert("method".into(), json!(text(6)));
    obj.insert("path".into(), json!(text(7)));
    obj.insert("endpointKind".into(), json!(text(8)));
    obj.insert("model".into(), json!(text(9)));
    obj.insert("provider".into(), json!(text(10)));
    obj.insert("resolvedModel".into(), json!(text(11)));
    obj.insert("status".into(), json!(int(12)));
    obj.insert("stream".into(), Value::Bool(int(13).unwrap_or(0) == 1));
    obj.insert("promptTokens".into(), json!(int(14)));
    obj.insert("completionTokens".into(), json!(int(15)));
    obj.insert("durationMs".into(), json!(int(16)));
    obj.insert("ttftMs".into(), json!(int(17)));
    obj.insert("tps".into(), json!(real(18)));
    obj.insert("userAgent".into(), json!(text(19)));
    obj.insert("error".into(), json!(text(20)));
    Ok(Value::Object(obj))
}

/// `clearRequestLogs()`: delete every row.
pub fn clear_request_logs(db: &Db) -> Result<()> {
    db.execute("DELETE FROM requestLogs")?;
    Ok(())
}

/// Get recent request logs (compact projection used by the dashboard).
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

// ---------------------------------------------------------------------------
// API key request slots (acquireApiKeyRequest)
// ---------------------------------------------------------------------------

const MINUTE_MS: i64 = 60_000;

#[derive(Default)]
struct KeyState {
    active_requests: i64,
    request_times: Vec<i64>,
}

static KEY_REQUEST_STATE: std::sync::LazyLock<Mutex<std::collections::HashMap<String, KeyState>>> =
    std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Acquire a request slot for an API key, honoring `rpm` and `limitConcurrent`.
///
/// Returns `{ok: true, ...}` or `{ok: false, reason: "rpm", retryAfter}`. The
/// JS version also queues concurrent waiters (FIFO with abort signals); this
/// port reports the busy state instead of blocking, since callers here are
/// synchronous DB helpers.
// ponytail: no FIFO queue/abort; add when a route needs to await a slot.
pub fn acquire_api_key_request(db: &Db, key: &Value) -> Result<Value> {
    acquire_api_key_request_at(db, key, now_ms())
}

fn acquire_api_key_request_at(_db: &Db, key: &Value, now: i64) -> Result<Value> {
    let key_id = key.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let rpm = key.get("rpm").and_then(Value::as_i64).unwrap_or(0);
    let limit_concurrent = key.get("limitConcurrent").and_then(Value::as_i64).unwrap_or(0);

    let mut states = KEY_REQUEST_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let state = states.entry(key_id).or_default();
    state.request_times.retain(|t| *t > now - MINUTE_MS);

    // RPM is checked immediately so queued requests do not silently starve it.
    if rpm > 0 && state.request_times.len() as i64 >= rpm {
        let oldest = state.request_times.first().copied().unwrap_or(now);
        let retry_after = ((oldest + MINUTE_MS - now) as f64 / 1000.0).ceil().max(1.0) as i64;
        return Ok(json!({
            "ok": false,
            "reason": "rpm",
            "retryAfter": retry_after,
        }));
    }

    if limit_concurrent > 0 && state.active_requests >= limit_concurrent {
        return Ok(json!({ "ok": false, "reason": "concurrent" }));
    }

    state.request_times.push(now);
    state.active_requests += 1;
    Ok(json!({ "ok": true, "release": Value::Null }))
}

/// Release a slot taken by `acquire_api_key_request` (JS `lease.release()`).
pub fn release_api_key_request(db: &Db, key_id: &str) -> Result<()> {
    release_api_key_request_at(db, key_id)
}

fn release_api_key_request_at(_db: &Db, key_id: &str) -> Result<()> {
    let mut states = KEY_REQUEST_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(state) = states.get_mut(key_id) {
        state.active_requests = (state.active_requests - 1).max(0);
    }
    Ok(())
}

/// Get API key monitoring stats.
pub fn get_api_key_monitoring(db: &Db, key_id: &str) -> Result<Value> {
    get_api_key_monitoring_at(db, key_id, now_ms())
}

fn get_api_key_monitoring_at(db: &Db, key_id: &str, now: i64) -> Result<Value> {
    let since = chrono::DateTime::from_timestamp_millis(now - 24 * 60 * 60 * 1000)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

    let row = db.query_row(
        "SELECT SUM(CASE WHEN timestamp >= ? THEN 1 ELSE 0 END) AS requestsLast24h, \
                SUM(CASE WHEN timestamp >= ? AND status >= 400 THEN 1 ELSE 0 END) AS errorsLast24h, \
                MAX(timestamp) AS lastRequestAt \
           FROM requestLogs WHERE apiKeyId = ?",
        &[&since, &since, &key_id],
        |row| {
            let requests: Option<i64> = row.get(0)?;
            let errors: Option<i64> = row.get(1)?;
            let last_request: Option<String> = row.get(2)?;
            Ok((requests, errors, last_request))
        },
    )?;

    let states = KEY_REQUEST_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (active, last_minute) = states
        .get(key_id)
        .map(|s| {
            (
                s.active_requests,
                s.request_times.iter().filter(|t| **t > now - MINUTE_MS).count() as i64,
            )
        })
        .unwrap_or((0, 0));

    let (requests, errors, last_request) = row.unwrap_or((None, None, None));
    Ok(json!({
        "activeRequests": active,
        "queuedRequests": 0,
        "requestsLastMinute": last_minute,
        "requestsLast24h": requests.unwrap_or(0),
        "errorsLast24h": errors.unwrap_or(0),
        "lastRequestAt": last_request,
    }))
}

/// Async local storage for request context.
///
/// The JS module attributes usage rows to the in-flight request log via
/// AsyncLocalStorage. Rust callers pass the context explicitly, so `run` is a
/// pass-through that keeps the call shape.
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

/// `recordUsageForRequestLog()`: attribute an entry's tokens to the request
/// context. No-op without an active context (Rust passes context explicitly).
pub fn record_usage_for_request_log(ctx: Option<&mut Value>, entry: &Value) -> bool {
    let Some(ctx) = ctx else {
        return false;
    };
    let tokens = entry.get("tokens").cloned().unwrap_or_else(|| json!({}));
    let pick = |keys: &[&str]| -> i64 {
        keys.iter()
            .find_map(|k| tokens.get(*k).and_then(Value::as_i64))
            .unwrap_or(0)
    };
    let prompt = pick(&["prompt_tokens", "input_tokens"]);
    let completion = pick(&["completion_tokens", "output_tokens"]);
    let signature = format!(
        "{}|{}|{}|{}|{}",
        entry.get("timestamp").and_then(Value::as_str).unwrap_or(""),
        entry.get("provider").and_then(Value::as_str).unwrap_or(""),
        entry.get("model").and_then(Value::as_str).unwrap_or(""),
        prompt,
        completion
    );

    let obj = match ctx.as_object_mut() {
        Some(o) => o,
        None => return false,
    };
    let seen = obj
        .entry("_seen")
        .or_insert_with(|| json!([]))
        .as_array_mut();
    if let Some(seen) = seen {
        if seen.iter().any(|v| v.as_str() == Some(signature.as_str())) {
            return false;
        }
        seen.push(Value::String(signature));
    }

    let add = |obj: &mut Map<String, Value>, field: &str, delta: i64| {
        let next = obj.get(field).and_then(Value::as_i64).unwrap_or(0) + delta;
        obj.insert(field.to_string(), json!(next));
    };
    add(obj, "promptTokens", prompt);
    add(obj, "completionTokens", completion);
    if let Some(provider) = entry.get("provider").and_then(Value::as_str) {
        if !provider.is_empty() {
            obj.insert("provider".into(), Value::String(provider.to_string()));
        }
    }
    if let Some(model) = entry.get("model").and_then(Value::as_str) {
        if !model.is_empty() {
            obj.insert("resolvedModel".into(), Value::String(model.to_string()));
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn db() -> (TempDir, Db) {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        (dir, db)
    }

    fn log_entry(ts: &str, model: &str) -> Value {
        json!({
            "timestamp": ts,
            "apiKeyId": "key-1",
            "apiKeyName": "Test key",
            "apiKeyMasked": "sk-123***abcd",
            "ip": "127.0.0.1",
            "method": "POST",
            "path": "/v1/chat/completions",
            "endpointKind": "chat",
            "model": model,
            "provider": "openai",
            "resolvedModel": model,
            "status": 200,
            "stream": true,
            "promptTokens": 12,
            "completionTokens": 34,
            "durationMs": 250,
            "ttftMs": 40,
            "tps": 12.5,
            "userAgent": "test-agent",
            "error": Value::Null,
        })
    }

    #[test]
    fn insert_then_query_roundtrip() {
        let (_dir, db) = db();
        insert_request_log(&db, &log_entry("2026-01-01T00:00:00.000Z", "gpt-5")).unwrap();

        let result = get_request_logs(&db, &RequestLogFilter::default()).unwrap();
        let logs = result.get("logs").and_then(Value::as_array).unwrap();
        assert_eq!(logs.len(), 1);
        let row = &logs[0];
        assert_eq!(row.get("model").and_then(Value::as_str), Some("gpt-5"));
        assert_eq!(row.get("provider").and_then(Value::as_str), Some("openai"));
        assert_eq!(row.get("status").and_then(Value::as_i64), Some(200));
        // stream is stored as 0/1 and returned as a bool
        assert_eq!(row.get("stream").and_then(Value::as_bool), Some(true));
        assert_eq!(row.get("tps").and_then(Value::as_f64), Some(12.5));

        let pagination = result.get("pagination").unwrap();
        assert_eq!(pagination.get("totalItems").and_then(Value::as_i64), Some(1));
        assert_eq!(pagination.get("totalPages").and_then(Value::as_i64), Some(1));
        assert_eq!(pagination.get("page").and_then(Value::as_i64), Some(1));
        assert_eq!(pagination.get("pageSize").and_then(Value::as_i64), Some(50));
    }

    #[test]
    fn insert_binds_null_for_missing_fields() {
        let (_dir, db) = db();
        insert_request_log(&db, &json!({"timestamp": "2026-01-01T00:00:00.000Z"})).unwrap();
        let row = db
            .query_row(
                "SELECT status, apiKeyId, error FROM requestLogs LIMIT 1",
                &[],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(row, (None, None, None));
    }

    #[test]
    fn filters_by_status_kind_search_and_key() {
        let (_dir, db) = db();
        insert_request_log(&db, &log_entry("2026-01-01T00:00:00.000Z", "gpt-5")).unwrap();
        let mut failed = log_entry("2026-01-01T00:00:01.000Z", "claude-opus-4.7");
        failed["status"] = json!(500);
        failed["endpointKind"] = json!("messages");
        failed["apiKeyId"] = Value::Null;
        insert_request_log(&db, &failed).unwrap();

        let ok = get_request_logs(
            &db,
            &RequestLogFilter {
                status: Some("ok".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ok["logs"].as_array().unwrap().len(), 1);
        assert_eq!(ok["logs"][0]["model"], json!("gpt-5"));

        let errors = get_request_logs(
            &db,
            &RequestLogFilter {
                status: Some("error".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(errors["logs"][0]["status"], json!(500));

        let by_kind = get_request_logs(
            &db,
            &RequestLogFilter {
                endpoint_kind: Some("messages".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(by_kind["pagination"]["totalItems"], json!(1));

        let by_search = get_request_logs(
            &db,
            &RequestLogFilter {
                search: Some("claude".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(by_search["logs"][0]["model"], json!("claude-opus-4.7"));

        // apiKeyId "none" targets NULL rows
        let no_key = get_request_logs(
            &db,
            &RequestLogFilter {
                api_key_id: Some("none".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(no_key["pagination"]["totalItems"], json!(1));

        let by_key = get_request_logs(
            &db,
            &RequestLogFilter {
                api_key_id: Some("key-1".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(by_key["logs"][0]["apiKeyId"], json!("key-1"));
    }

    #[test]
    fn pagination_clamps_page_size() {
        let (_dir, db) = db();
        for i in 0..3 {
            insert_request_log(&db, &log_entry(&format!("2026-01-01T00:00:0{i}.000Z"), "gpt-5"))
                .unwrap();
        }
        let result = get_request_logs(
            &db,
            &RequestLogFilter {
                page: Some(2),
                page_size: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(result["logs"].as_array().unwrap().len(), 1);
        assert_eq!(result["pagination"]["page"], json!(2));
        assert_eq!(result["pagination"]["totalPages"], json!(2));

        // pageSize above 200 clamps, page below 1 clamps.
        let clamped = get_request_logs(
            &db,
            &RequestLogFilter {
                page: Some(0),
                page_size: Some(10_000),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(clamped["pagination"]["page"], json!(1));
        assert_eq!(clamped["pagination"]["pageSize"], json!(200));
    }

    #[test]
    fn compat_provider_is_labelled_with_node_name() {
        let (_dir, db) = db();
        let provider = "openai-compatible-0123abcd-node-uuid";
        crate::repos::nodes::create_provider_node(
            &db,
            &json!({"id": provider, "type": "openai-compatible", "name": "My Node"}),
        )
        .unwrap();
        let mut entry = log_entry("2026-01-01T00:00:00.000Z", "gpt-5");
        entry["provider"] = json!(provider);
        insert_request_log(&db, &entry).unwrap();

        let result = get_request_logs(&db, &RequestLogFilter::default()).unwrap();
        assert_eq!(result["logs"][0]["provider"], json!("My Node"));
    }

    #[test]
    fn compat_provider_falls_back_to_short_id() {
        let (_dir, db) = db();
        let mut entry = log_entry("2026-01-01T00:00:00.000Z", "gpt-5");
        entry["provider"] = json!("openai-compatible-0123abcd-nope");
        insert_request_log(&db, &entry).unwrap();
        let result = get_request_logs(&db, &RequestLogFilter::default()).unwrap();
        assert_eq!(result["logs"][0]["provider"], json!("openai-compatible-nope"));
    }

    #[test]
    fn clear_removes_all_rows() {
        let (_dir, db) = db();
        insert_request_log(&db, &log_entry("2026-01-01T00:00:00.000Z", "gpt-5")).unwrap();
        assert_eq!(get_recent_logs(&db, 10).unwrap().len(), 1);
        clear_request_logs(&db).unwrap();
        assert_eq!(get_recent_logs(&db, 10).unwrap().len(), 0);
    }

    #[test]
    fn prune_keeps_newest_rows_after_interval() {
        let (_dir, db) = db();
        // Force the prune window: insert MAX_ROWS + 1 rows directly, then trigger
        // one more insert through the public API.
        let mut stmt = db
            .execute(
                "INSERT INTO requestLogs(timestamp, model) SELECT '2026-01-01T00:00:00.000Z', 'm' FROM (WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 5001) SELECT n FROM seq)",
            )
            .unwrap();
        assert_eq!(stmt, 5001);
        stmt = 0;
        let _ = stmt;

        // Jump the counter to one before the prune threshold.
        INSERTS_SINCE_PRUNE.store(PRUNE_EVERY - 1, Ordering::Relaxed);
        insert_request_log(&db, &log_entry("2026-01-01T00:00:09.000Z", "newest")).unwrap();

        let count: i64 = db
            .query_row("SELECT COUNT(*) FROM requestLogs", &[], |row| row.get(0))
            .unwrap()
            .unwrap();
        assert_eq!(count, MAX_ROWS);
        // The just-inserted row survived.
        let newest = db
            .query_row(
                "SELECT model FROM requestLogs ORDER BY id DESC LIMIT 1",
                &[],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
            .unwrap();
        assert_eq!(newest, "newest");
    }

    #[test]
    fn null_bytes_are_stripped_before_insert() {
        let (_dir, db) = db();
        let mut entry = log_entry("2026-01-01T00:00:00.000Z", "gpt-5");
        entry["userAgent"] = json!("agent\0with\0nul");
        insert_request_log(&db, &entry).unwrap();
        let ua = db
            .query_row("SELECT userAgent FROM requestLogs LIMIT 1", &[], |row| {
                row.get::<_, String>(0)
            })
            .unwrap()
            .unwrap();
        assert_eq!(ua, "agentwithnul");
    }

    #[test]
    fn api_key_monitoring_counts_24h_window() {
        let (_dir, db) = db();
        let now = now_ms();
        let recent = chrono::DateTime::from_timestamp_millis(now - 1000)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let stale = chrono::DateTime::from_timestamp_millis(now - 48 * 60 * 60 * 1000)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

        let mut ok = log_entry(&recent, "gpt-5");
        ok["apiKeyId"] = json!("key-mon");
        insert_request_log(&db, &ok).unwrap();

        let mut err = log_entry(&recent, "gpt-5");
        err["status"] = json!(429);
        err["apiKeyId"] = json!("key-mon");
        insert_request_log(&db, &err).unwrap();

        let mut old = log_entry(&stale, "gpt-5");
        old["apiKeyId"] = json!("key-mon");
        insert_request_log(&db, &old).unwrap();

        let stats = get_api_key_monitoring_at(&db, "key-mon", now).unwrap();
        assert_eq!(stats["requestsLast24h"], json!(2));
        assert_eq!(stats["errorsLast24h"], json!(1));
        assert_eq!(stats["lastRequestAt"], json!(recent));
    }

    #[test]
    fn acquire_slot_enforces_rpm() {
        let (_dir, db) = db();
        let now = now_ms();
        let key = json!({"id": "k1", "rpm": 2});

        assert_eq!(
            acquire_api_key_request_at(&db, &key, now).unwrap()["ok"],
            json!(true)
        );
        assert_eq!(
            acquire_api_key_request_at(&db, &key, now).unwrap()["ok"],
            json!(true)
        );
        let denied = acquire_api_key_request_at(&db, &key, now).unwrap();
        assert_eq!(denied["ok"], json!(false));
        assert_eq!(denied["reason"], json!("rpm"));
        assert!(denied["retryAfter"].as_i64().unwrap() >= 1);

        // A minute later the window is clear again.
        assert_eq!(
            acquire_api_key_request_at(&db, &key, now + MINUTE_MS + 1).unwrap()["ok"],
            json!(true)
        );
    }

    #[test]
    fn acquire_slot_enforces_concurrency_and_release() {
        let (_dir, db) = db();
        let now = now_ms();
        let key = json!({"id": "k2", "limitConcurrent": 1});

        assert_eq!(
            acquire_api_key_request_at(&db, &key, now).unwrap()["ok"],
            json!(true)
        );
        let denied = acquire_api_key_request_at(&db, &key, now).unwrap();
        assert_eq!(denied["ok"], json!(false));
        assert_eq!(denied["reason"], json!("concurrent"));

        release_api_key_request_at(&db, "k2").unwrap();
        assert_eq!(
            acquire_api_key_request_at(&db, &key, now).unwrap()["ok"],
            json!(true)
        );
    }

    #[test]
    fn unlimited_key_always_grants() {
        let (_dir, db) = db();
        let key = json!({"id": "k3", "rpm": 0, "limitConcurrent": 0});
        for _ in 0..5 {
            assert_eq!(
                acquire_api_key_request(&db, &key).unwrap()["ok"],
                json!(true)
            );
        }
    }

    #[test]
    fn record_usage_dedupes_and_sums() {
        let mut ctx = json!({"promptTokens": 0, "completionTokens": 0, "_seen": []});
        let entry = json!({
            "timestamp": "2026-01-01T00:00:00.000Z",
            "provider": "openai",
            "model": "gpt-5",
            "tokens": {"prompt_tokens": 10, "completion_tokens": 5}
        });

        assert!(record_usage_for_request_log(Some(&mut ctx), &entry));
        assert_eq!(ctx["promptTokens"], json!(10));
        assert_eq!(ctx["completionTokens"], json!(5));
        assert_eq!(ctx["provider"], json!("openai"));
        assert_eq!(ctx["resolvedModel"], json!("gpt-5"));

        // Same signature is ignored on the second call.
        assert!(!record_usage_for_request_log(Some(&mut ctx), &entry));
        assert_eq!(ctx["promptTokens"], json!(10));

        // No context -> no-op.
        assert!(!record_usage_for_request_log(None, &entry));
    }

    #[test]
    fn recent_logs_projection() {
        let (_dir, db) = db();
        insert_request_log(&db, &log_entry("2026-01-01T00:00:00.000Z", "gpt-5")).unwrap();
        let logs = get_recent_logs(&db, 10).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0]["model"], json!("gpt-5"));
        assert_eq!(logs[0]["status"], json!(200));
        assert_eq!(logs[0]["durationMs"], json!(250));
    }

    #[test]
    fn provider_label_rules() {
        let nodes = Map::new();
        let mut aliases = Map::new();
        aliases.insert("my-alias".into(), json!("openai/gpt-5"));
        assert_eq!(provider_label("my-alias", &nodes, &aliases), "openai");
        assert_eq!(provider_label("", &nodes, &aliases), "");
        assert_eq!(provider_label("openai", &nodes, &aliases), "openai");
        // Non-compat providers pass through untouched.
        assert_eq!(provider_label("anthropic/claude-opus-4.7", &nodes, &aliases), "anthropic/claude-opus-4.7");
        // The node name wins over the compat-id short form (last 4 chars of the id).
        assert_eq!(
            provider_label("openai-compatible-responses-deadbeef-x", &nodes, &aliases),
            "openai-compatible-ef-x"
        );
        let mut nodes = Map::new();
        nodes.insert("openai-compatible-responses-deadbeef-x".into(), json!("Node One"));
        assert_eq!(
            provider_label("openai-compatible-responses-deadbeef-x", &nodes, &aliases),
            "Node One"
        );
    }

    #[test]
    fn filter_from_json_ignores_empty_strings() {
        let f = RequestLogFilter::from_json(Some(&json!({
            "apiKeyId": "",
            "status": "ok",
            "page": 3,
            "pageSize": 10
        })));
        assert!(f.api_key_id.is_none());
        assert_eq!(f.status.as_deref(), Some("ok"));
        assert_eq!(f.page, Some(3));
        assert_eq!(f.page_size, Some(10));
    }
}
