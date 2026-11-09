use crate::connection::Db;
use crate::helpers::{parse_json, stringify_json};
use nr_core::Result;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::Timelike;

// In-memory state for pending requests and stats
lazy_static::lazy_static! {
    static ref PENDING_REQUESTS: Arc<Mutex<PendingState>> = {
        Arc::new(Mutex::new(PendingState::new()))
    };
}

struct PendingState {
    by_model: HashMap<String, i32>,
    by_account: HashMap<String, HashMap<String, i32>>,
    last_error_provider: (String, i64),
}

impl PendingState {
    fn new() -> Self {
        PendingState {
            by_model: HashMap::new(),
            by_account: HashMap::new(),
            last_error_provider: (String::new(), 0),
        }
    }
}

const PENDING_TIMEOUT_MS: u64 = 60 * 1000;
const RING_CAP: usize = 50;
const PERIOD_MS: &[(&str, u64)] = &[
    ("24h", 86400000),
    ("7d", 604800000),
    ("30d", 2592000000),
    ("60d", 5184000000),
];

fn get_local_date_key(timestamp: Option<i64>) -> String {
    let ts = match timestamp {
        Some(ts) => match chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ts) {
            Some(dt) => dt,
            None => chrono::Utc::now(),
        },
        None => chrono::Utc::now(),
    };
    let local: chrono::DateTime<chrono::Local> = ts.into();
    local.format("%Y-%m-%d").to_string()
}

/// Mask an API key for display
fn mask_api_key(key: Option<&str>) -> Option<String> {
    key.and_then(|k| {
        let trimmed = k.trim();
        if trimmed.is_empty() {
            None
        } else if trimmed.len() <= 12 {
            Some(format!("{}***", trimmed.chars().next().unwrap_or('?')))
        } else {
            Some(format!("{}***{}", &trimmed[..8], &trimmed[trimmed.len() - 4..]))
        }
    })
}

/// Track a pending request (increment/decrement counter)
pub fn track_pending_request(
    _db: &Db,
    model: &str,
    provider: &str,
    _connection_id: Option<&str>,
    started: bool,
    _error: bool,
) -> Result<()> {
    let mut state = PENDING_REQUESTS.lock().unwrap();
    let model_key = if !provider.is_empty() {
        format!("{} ({})", model, provider)
    } else {
        model.to_string()
    };

    if started {
        let entry = state.by_model.entry(model_key).or_insert(0);
        *entry += 1;
    } else {
        if let Some(entry) = state.by_model.get_mut(&model_key) {
            *entry = (*entry - 1).max(0);
        }
    }

    // Note: stats event emission would go to dashboard via broadcast channel
    // TODO(dedupe): Integrate tokio broadcast when async runtime is available
    Ok(())
}

/// Get active requests summary
pub fn get_active_requests(db: &Db) -> Result<Value> {
    let state = PENDING_REQUESTS.lock().unwrap();

    let mut active_requests = vec![];
    for (model_key, count) in &state.by_model {
        if *count > 0 {
            if let Some((model, provider)) = model_key.split_once(" (") {
                let provider = provider.trim_end_matches(')');
                active_requests.push(json!({
                    "model": model,
                    "provider": provider,
                    "account": "unknown",
                    "count": count
                }));
            }
        }
    }

    // Get recent requests from database
    let mut recent_requests = vec![];
    let rows = db.query_all(
        "SELECT timestamp, provider, model, promptTokens, completionTokens, status, tokens FROM usageHistory ORDER BY id DESC LIMIT 50",
        &[],
        |row| {
            Ok(json!({
                "timestamp": row.get::<_, String>(0)?,
                "provider": row.get::<_, Option<String>>(1)?,
                "model": row.get::<_, Option<String>>(2)?,
                "promptTokens": row.get::<_, i64>(3)?,
                "completionTokens": row.get::<_, i64>(4)?,
                "cachedTokens": 0,
                "cost": 0.0,
                "status": row.get::<_, Option<String>>(5)?,
            }))
        },
    )?;

    // Deduplicate recent requests by model/provider/tokens
    let mut seen = std::collections::HashSet::new();
    for entry in rows {
        if let (Some(model), Some(provider), Some(ts)) = (
            entry.get("model").and_then(|v| v.as_str()),
            entry.get("provider").and_then(|v| v.as_str()),
            entry.get("timestamp").and_then(|v| v.as_str()),
        ) {
            let minute = &ts[0..16];
            let pt = entry.get("promptTokens").and_then(|v| v.as_i64()).unwrap_or(0);
            let ct = entry.get("completionTokens").and_then(|v| v.as_i64()).unwrap_or(0);
            let key = format!("{}|{}|{}|{}|{}", model, provider, pt, ct, minute);
            if seen.insert(key) {
                recent_requests.push(entry);
                if recent_requests.len() >= 20 {
                    break;
                }
            }
        }
    }

    let error_provider = if chrono::Utc::now().timestamp_millis() - state.last_error_provider.1 < 10000 {
        state.last_error_provider.0.clone()
    } else {
        String::new()
    };

    Ok(json!({
        "activeRequests": active_requests,
        "recentRequests": recent_requests,
        "errorProvider": error_provider
    }))
}

/// Calculate cost from tokens and pricing
async fn calculate_cost(provider: Option<&str>, model: Option<&str>, tokens: Option<&Value>) -> f64 {
    if tokens.is_none() || provider.is_none() || model.is_none() {
        return 0.0;
    }
    // TODO(dedupe): Import calculate_cost_from_tokens from nr_data once ported
    0.0
}

/// Save usage entry to the database with deduplication
pub async fn save_request_usage(db: &Db, entry: &Value) -> Result<()> {
    let timestamp = entry
        .get("timestamp")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        });

    let provider = entry.get("provider").and_then(|v| v.as_str());
    let model = entry.get("model").and_then(|v| v.as_str());
    let connection_id = entry.get("connectionId").and_then(|v| v.as_str());
    let api_key = entry.get("apiKey").and_then(|v| v.as_str());
    let endpoint = entry.get("endpoint").and_then(|v| v.as_str());
    let status = entry.get("status").and_then(|v| v.as_str()).unwrap_or("ok");

    let default_tokens = json!({});
    let tokens = entry.get("tokens").unwrap_or(&default_tokens);
    let prompt_tokens = tokens
        .get("prompt_tokens")
        .or_else(|| tokens.get("input_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let completion_tokens = tokens
        .get("completion_tokens")
        .or_else(|| tokens.get("output_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let cached_tokens = tokens
        .get("cached_tokens")
        .or_else(|| tokens.get("cache_read_input_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    // Calculate cost
    let cost = calculate_cost(provider, model, Some(tokens)).await;

    // Transaction: check for duplicate, insert, update daily, increment lifetime counter
    db.transaction(|db| {
        // Check for existing row with same key columns
        let existing = db.query_row(
            "SELECT id FROM usageHistory WHERE timestamp = ? AND COALESCE(provider, '') = COALESCE(?, '') AND COALESCE(model, '') = COALESCE(?, '') AND COALESCE(connectionId, '') = COALESCE(?, '') AND COALESCE(apiKey, '') = COALESCE(?, '') AND promptTokens = ? AND completionTokens = ? ORDER BY id DESC LIMIT 1",
            &[
                &timestamp.as_str() as &dyn rusqlite::ToSql,
                &provider,
                &model,
                &connection_id,
                &api_key,
                &(prompt_tokens as i32),
                &(completion_tokens as i32),
            ],
            |_row| Ok(1),
        )?;

        if existing.is_some() {
            // Duplicate found - only update endpoint if it was missing
            if let Some(ep) = endpoint {
                db.execute_with_params(
                    "UPDATE usageHistory SET endpoint = ? WHERE timestamp = ? AND COALESCE(provider, '') = COALESCE(?, '')",
                    (ep, timestamp.as_str(), provider),
                )?;
            }
            return Ok(());
        }

        let tokens_json = stringify_json(tokens);
        let empty_meta = json!({});
        let meta_json = stringify_json(&empty_meta);

        // Insert new usage history entry
        db.execute_with_params(
            "INSERT INTO usageHistory(timestamp, provider, model, connectionId, apiKey, endpoint, promptTokens, completionTokens, cost, status, tokens, meta) VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (
                timestamp.as_str(),
                &provider,
                &model,
                &connection_id,
                &api_key,
                &endpoint,
                &(prompt_tokens as i32),
                &(completion_tokens as i32),
                &cost,
                &status,
                &tokens_json,
                &meta_json,
            ),
        )?;

        // Update daily aggregate
        let date_key = get_local_date_key(None);
        let existing_day = db.query_row(
            "SELECT data FROM usageDaily WHERE dateKey = ?",
            &[&date_key as &dyn rusqlite::ToSql],
            |row| row.get::<_, String>(0),
        )?;

        let mut day_data = if let Some(data_str) = existing_day {
            parse_json(Some(data_str), json!({}))
        } else {
            json!({
                "requests": 0,
                "promptTokens": 0,
                "completionTokens": 0,
                "cachedTokens": 0,
                "cost": 0.0,
                "byProvider": {},
                "byModel": {},
                "byAccount": {},
                "byApiKey": {},
                "byEndpoint": {}
            })
        };

        // Aggregate entry into day
        if let Some(obj) = day_data.as_object_mut() {
            obj["requests"] = json!(obj.get("requests").and_then(|v| v.as_i64()).unwrap_or(0) + 1);
            obj["promptTokens"] = json!(obj.get("promptTokens").and_then(|v| v.as_i64()).unwrap_or(0) + prompt_tokens);
            obj["completionTokens"] = json!(obj.get("completionTokens").and_then(|v| v.as_i64()).unwrap_or(0) + completion_tokens);
            obj["cachedTokens"] = json!(obj.get("cachedTokens").and_then(|v| v.as_i64()).unwrap_or(0) + cached_tokens);
            obj["cost"] = json!(obj.get("cost").and_then(|v| v.as_f64()).unwrap_or(0.0) + cost);
        }

        let day_json = stringify_json(&day_data);
        db.execute_with_params(
            "INSERT INTO usageDaily(dateKey, data) VALUES(?, ?) ON CONFLICT(dateKey) DO UPDATE SET data = excluded.data",
            (&date_key, &day_json),
        )?;

        // Increment lifetime counter
        let current = db.query_row(
            "SELECT value FROM _meta WHERE key = 'totalRequestsLifetime'",
            &[],
            |row| row.get::<_, String>(0),
        )?;
        let next = current
            .and_then(|v| v.parse::<i32>().ok())
            .unwrap_or(0) + 1;
        db.execute_with_params(
            "INSERT INTO _meta(key, value) VALUES('totalRequestsLifetime', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            (&String::from("totalRequestsLifetime"), &next.to_string()),
        )?;

        // Note: stats event emission would go to dashboard via broadcast channel
        // TODO(dedupe): Integrate tokio broadcast when async runtime is available
        Ok(())
    })?;

    Ok(())
}

/// Get usage history with optional filtering
pub fn get_usage_history(db: &Db, filter: Option<&Value>) -> Result<Vec<Value>> {
    let mut where_clauses = vec![];
    let mut params: Vec<String> = vec![];

    if let Some(f) = filter {
        if let Some(provider) = f.get("provider").and_then(|v| v.as_str()) {
            where_clauses.push("provider = ?");
            params.push(provider.to_string());
        }
        if let Some(model) = f.get("model").and_then(|v| v.as_str()) {
            where_clauses.push("model = ?");
            params.push(model.to_string());
        }
    }

    let where_sql = if where_clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_clauses.join(" AND "))
    };

    let sql = format!(
        "SELECT timestamp, provider, model, connectionId, apiKey, endpoint, cost, status, tokens FROM usageHistory{} ORDER BY id ASC",
        where_sql
    );

    let params_ref: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p as &dyn rusqlite::ToSql).collect();

    db.query_all(&sql, &params_ref, |row| {
        let tokens_str: Option<String> = row.get(8).ok();
        let tokens = parse_json(tokens_str, json!({}));

        Ok(json!({
            "timestamp": row.get::<_, String>(0)?,
            "provider": row.get::<_, Option<String>>(1)?,
            "model": row.get::<_, Option<String>>(2)?,
            "connectionId": row.get::<_, Option<String>>(3)?,
            "apiKeyMasked": mask_api_key(row.get::<_, Option<String>>(4)?.as_deref()),
            "endpoint": row.get::<_, Option<String>>(5)?,
            "cost": row.get::<_, f64>(6)?,
            "status": row.get::<_, Option<String>>(7)?,
            "tokens": tokens,
        }))
    })
}

/// Get usage statistics for a period
pub fn get_usage_stats(db: &Db, period: Option<&str>) -> Result<Value> {
    let period = period.unwrap_or("all");

    let mut stats = json!({
        "totalRequests": 0,
        "totalPromptTokens": 0,
        "totalCompletionTokens": 0,
        "totalCachedTokens": 0,
        "totalCost": 0.0,
        "byProvider": {},
        "byModel": {},
        "byAccount": {},
        "byApiKey": {},
        "byEndpoint": {},
        "last10Minutes": [],
        "pending": {},
        "activeRequests": [],
        "recentRequests": [],
        "errorProvider": ""
    });

    // Load period data from usageDaily
    if period != "24h" && period != "today" {
        let (max_days, use_daily) = match period {
            "7d" => (Some(7), true),
            "30d" => (Some(30), true),
            "60d" => (Some(60), true),
            _ => (None, true), // "all"
        };

        if use_daily {
            let sql = if let Some(days) = max_days {
                let today = chrono::Local::now().date_naive();
                let cutoff = today - chrono::Duration::days(days as i64 - 1);
                let cutoff_key = cutoff.format("%Y-%m-%d").to_string();
                format!(
                    "SELECT dateKey, data FROM usageDaily WHERE dateKey >= '{}' ORDER BY dateKey ASC",
                    cutoff_key
                )
            } else {
                "SELECT dateKey, data FROM usageDaily ORDER BY dateKey ASC".to_string()
            };

            let day_rows = db.query_all(&sql, &[], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                ))
            })?;

            for (_, data_str) in day_rows {
                let day = parse_json(Some(data_str), json!({}));
                if let Some(day_obj) = day.as_object() {
                    if let Some(stats_obj) = stats.as_object_mut() {
                        stats_obj["totalPromptTokens"] = json!(
                            stats_obj.get("totalPromptTokens").and_then(|v| v.as_i64()).unwrap_or(0)
                                + day_obj.get("promptTokens").and_then(|v| v.as_i64()).unwrap_or(0)
                        );
                        stats_obj["totalCompletionTokens"] = json!(
                            stats_obj.get("totalCompletionTokens").and_then(|v| v.as_i64()).unwrap_or(0)
                                + day_obj.get("completionTokens").and_then(|v| v.as_i64()).unwrap_or(0)
                        );
                        stats_obj["totalCost"] = json!(
                            stats_obj.get("totalCost").and_then(|v| v.as_f64()).unwrap_or(0.0)
                                + day_obj.get("cost").and_then(|v| v.as_f64()).unwrap_or(0.0)
                        );
                    }
                }
            }
        }
    }

    Ok(stats)
}

/// Get realtime summary for a period
pub fn get_realtime_summary(db: &Db, period: Option<&str>) -> Result<Value> {
    let period = period.unwrap_or("today");

    let mut summary = json!({
        "totalRequests": 0,
        "totalPromptTokens": 0,
        "totalCompletionTokens": 0,
        "totalCachedTokens": 0,
        "totalCost": 0.0
    });

    let cutoff = match period {
        "today" => {
            let now = chrono::Local::now();
            let start_of_day = now
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .unwrap_or_default();
            chrono::DateTime::<chrono::Local>::from_naive_utc_and_offset(
                start_of_day,
                now.offset().to_owned(),
            )
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        }
        "24h" => {
            let now = chrono::Utc::now();
            let before = now - chrono::Duration::hours(24);
            before.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        }
        _ => return Ok(summary),
    };

    let rows = db.query_all(
        "SELECT promptTokens, completionTokens, cost, tokens FROM usageHistory WHERE timestamp >= ? ORDER BY id ASC",
        &[&cutoff as &dyn rusqlite::ToSql],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, f64>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        },
    )?;

    let mut total_req = 0;
    let mut total_prompt = 0i64;
    let mut total_completion = 0i64;
    let mut total_cached = 0i64;
    let mut total_cost = 0.0;

    for (prompt_tokens, completion_tokens, cost, tokens_str) in rows {
        total_req += 1;
        total_prompt += prompt_tokens;
        total_completion += completion_tokens;

        if let Some(ts) = tokens_str {
            let tokens = parse_json(Some(ts), json!({}));
            if let Some(cached) = tokens.get("cached_tokens").and_then(|v| v.as_i64()) {
                total_cached += cached;
            }
        }
        total_cost += cost;
    }

    summary["totalRequests"] = json!(total_req);
    summary["totalPromptTokens"] = json!(total_prompt);
    summary["totalCompletionTokens"] = json!(total_completion);
    summary["totalCachedTokens"] = json!(total_cached);
    summary["totalCost"] = json!(total_cost);

    Ok(summary)
}

/// Get chart data for visualization
pub fn get_chart_data(db: &Db, period: Option<&str>) -> Result<Vec<Value>> {
    let period = period.unwrap_or("7d");
    let mut buckets = vec![];

    match period {
        "today" => {
            let now = chrono::Local::now();
            let start_of_day = now
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .unwrap_or_default();
            let start_time = chrono::DateTime::<chrono::Local>::from_naive_utc_and_offset(
                start_of_day,
                now.offset().to_owned(),
            );

            for i in 0..24 {
                let bucket_time = start_time + chrono::Duration::hours(i);
                let label = bucket_time.format("%H:%M").to_string();
                buckets.push(json!({
                    "label": label,
                    "tokens": 0,
                    "cost": 0.0,
                    "requests": 0
                }));
            }

            let rows = db.query_all(
                "SELECT timestamp, promptTokens, completionTokens, cost FROM usageHistory WHERE timestamp >= ?",
                &[&start_time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true) as &dyn rusqlite::ToSql],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, f64>(3)?,
                    ))
                },
            )?;

            for (ts, prompt_tokens, completion_tokens, cost) in rows {
                if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&ts) {
                    let hour = dt.hour() as usize;
                    if hour < 24 {
                        if let Some(bucket) = buckets.get_mut(hour) {
                            if let Some(b) = bucket.as_object_mut() {
                                b["tokens"] = json!(
                                    b.get("tokens").and_then(|v| v.as_i64()).unwrap_or(0)
                                        + prompt_tokens + completion_tokens
                                );
                                b["cost"] = json!(
                                    b.get("cost").and_then(|v| v.as_f64()).unwrap_or(0.0) + cost
                                );
                                b["requests"] = json!(
                                    b.get("requests").and_then(|v| v.as_i64()).unwrap_or(0) + 1
                                );
                            }
                        }
                    }
                }
            }
        }
        "7d" | "30d" | "60d" => {
            let days = match period {
                "7d" => 7,
                "30d" => 30,
                "60d" => 60,
                _ => 7,
            };

            let today = chrono::Local::now().date_naive();
            for i in 0..days {
                let d = today - chrono::Duration::days((days - 1 - i) as i64);
                let label = d.format("%b %d").to_string();
                buckets.push(json!({
                    "label": label,
                    "tokens": 0,
                    "cost": 0.0,
                    "requests": 0
                }));
            }

            // Load from usageDaily
            let day_rows = db.query_all(
                "SELECT dateKey, data FROM usageDaily ORDER BY dateKey ASC",
                &[],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                    ))
                },
            )?;

            let mut day_map: HashMap<String, Value> = HashMap::new();
            for (key, data_str) in day_rows {
                day_map.insert(key, parse_json(Some(data_str), json!({})));
            }

            for (i, bucket) in buckets.iter_mut().enumerate() {
                let d = today - chrono::Duration::days((days - 1 - i as i32) as i64);
                let date_key = d.format("%Y-%m-%d").to_string();
                if let Some(day_data) = day_map.get(&date_key) {
                    if let Some(b) = bucket.as_object_mut() {
                        b["tokens"] = json!(
                            day_data.get("promptTokens").and_then(|v| v.as_i64()).unwrap_or(0)
                                + day_data.get("completionTokens").and_then(|v| v.as_i64()).unwrap_or(0)
                        );
                        b["cost"] = json!(day_data.get("cost").and_then(|v| v.as_f64()).unwrap_or(0.0));
                        b["requests"] = json!(day_data.get("requests").and_then(|v| v.as_i64()).unwrap_or(0));
                    }
                }
            }
        }
        _ => {} // "all" or other
    }

    Ok(buckets)
}

/// Get recent log entries (text format)
pub fn get_recent_logs(db: &Db, limit: usize) -> Result<Vec<String>> {
    let rows = db.query_all(
        "SELECT timestamp, provider, model, connectionId, promptTokens, completionTokens, status, tokens FROM usageHistory ORDER BY id DESC LIMIT ?",
        &[&(limit as i32) as &dyn rusqlite::ToSql],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        },
    )?;

    let mut logs = vec![];
    for (ts, provider, model, conn_id, prompt_tokens, completion_tokens, status, _tokens_str) in
        rows.into_iter().rev()
    {
        let ts_obj = chrono::DateTime::parse_from_rfc3339(&ts).ok();
        let formatted_ts = ts_obj
            .map(|t| {
                t.format("%d-%m-%Y %H:%M:%S").to_string()
            })
            .unwrap_or_else(|| ts.clone());

        let p = provider.as_deref().unwrap_or("-").to_uppercase();
        let m = model.as_deref().unwrap_or("-");
        let account = conn_id
            .as_deref()
            .map(|id| &id[..8.min(id.len())])
            .unwrap_or("-");
        let sent = prompt_tokens;
        let received = completion_tokens;
        let st = status.as_deref().unwrap_or("-");

        logs.push(format!(
            "{} | {} | {} | {} | {} | {} | {}",
            formatted_ts, m, p, account, sent, received, st
        ));
    }

    Ok(logs)
}

/// Get stats emitter (would emit updates to dashboard via broadcast channel)
/// TODO(dedupe): Implement tokio broadcast integration once async runtime is available
pub fn get_stats_emitter() -> () {
    () // Placeholder - returns unit type for now
}
