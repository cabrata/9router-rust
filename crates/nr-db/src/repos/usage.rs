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
    pending_timers: HashMap<String, tokio::task::JoinHandle<()>>,
    last_error_provider: (String, i64),
}

impl PendingState {
    fn new() -> Self {
        PendingState {
            by_model: HashMap::new(),
            by_account: HashMap::new(),
            pending_timers: HashMap::new(),
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

/// Track a pending request (increment/decrement counter).
/// Mirrors usageRepo.trackPendingRequest: byModel/byAccount counts, a
/// PENDING_TIMEOUT_MS watchdog that clears a stuck entry, and the 10s
/// lastErrorProvider window surfaced by getActiveRequests().
pub fn track_pending_request(
    _db: &Db,
    model: &str,
    provider: &str,
    connection_id: Option<&str>,
    started: bool,
    error: bool,
) -> Result<()> {
    let model_key = if !provider.is_empty() {
        format!("{} ({})", model, provider)
    } else {
        model.to_string()
    };
    let timer_key = format!("{}|{}", connection_id.unwrap_or(""), model_key);

    {
        let mut state = PENDING_REQUESTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let bump = |slot: &mut i32, delta: i32| {
            *slot = (*slot + delta).max(0);
        };

        let model_slot = state.by_model.entry(model_key.clone()).or_insert(0);
        bump(model_slot, if started { 1 } else { -1 });
        if *model_slot == 0 {
            state.by_model.remove(&model_key);
        }

        if let Some(conn_id) = connection_id.filter(|id| !id.is_empty()) {
            let models = state.by_account.entry(conn_id.to_string()).or_default();
            let slot = models.entry(model_key.clone()).or_insert(0);
            bump(slot, if started { 1 } else { -1 });
            if *slot == 0 {
                models.remove(&model_key);
                if models.is_empty() {
                    state.by_account.remove(conn_id);
                }
            }
        }

        // Watchdog: clear a pending entry if its stream never finished.
        if started {
            let task_model_key = model_key.clone();
            let task_conn_id = connection_id.map(str::to_string);
            let handle = tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(PENDING_TIMEOUT_MS)).await;
                let mut state = PENDING_REQUESTS
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(slot) = state.by_model.get_mut(&task_model_key) {
                    *slot = 0;
                }
                if let Some(conn_id) = task_conn_id {
                    if let Some(models) = state.by_account.get_mut(&conn_id) {
                        if let Some(slot) = models.get_mut(&task_model_key) {
                            *slot = 0;
                        }
                    }
                }
            });
            state.pending_timers.insert(timer_key, handle);
        } else {
            // clearTimeout: the request finished; drop the watchdog.
            if let Some(handle) = state.pending_timers.remove(&timer_key) {
                handle.abort();
            }
        }

        // A failed request darkens that provider for 10s.
        if !started && error && !provider.is_empty() {
            state.last_error_provider = (provider.to_lowercase(), chrono::Utc::now().timestamp_millis());
        }
    }

    schedule_stats_event("pending", 150);
    Ok(())
}

/// Get active requests summary
pub fn get_active_requests(db: &Db) -> Result<Value> {
    // Connection display names, like getConnectionMapCached().
    let connection_map: HashMap<String, String> = crate::repos::connections::get_provider_connections(db, None)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|c| {
            let id = c.get("id").and_then(|v| v.as_str())?.to_string();
            let name = c
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .or_else(|| c.get("email").and_then(|v| v.as_str()).filter(|s| !s.is_empty()))
                .unwrap_or(&id)
                .to_string();
            Some((id, name))
        })
        .collect();

    let error_provider = {
        let state = PENDING_REQUESTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let recent = chrono::Utc::now().timestamp_millis() - state.last_error_provider.1 < 10000;
        if recent {
            state.last_error_provider.0.clone()
        } else {
            String::new()
        }
    };

    let mut active_requests = vec![];
    {
        let state = PENDING_REQUESTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        for (connection_id, models) in &state.by_account {
            for (model_key, count) in models {
                if *count <= 0 {
                    continue;
                }
                let account_name = connection_map
                    .get(connection_id)
                    .cloned()
                    .unwrap_or_else(|| {
                        let short: String = connection_id.chars().take(8).collect();
                        format!("Account {short}...")
                    });
                let (model, provider) = match model_key.rsplit_once(" (") {
                    Some((model, provider)) => {
                        (model.to_string(), provider.trim_end_matches(')').to_string())
                    }
                    None => (model_key.clone(), "unknown".to_string()),
                };
                active_requests.push(json!({
                    "model": model,
                    "provider": provider,
                    "account": account_name,
                    "count": count,
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

    Ok(json!({
        "activeRequests": active_requests,
        "recentRequests": recent_requests,
        "errorProvider": error_provider
    }))
}

/// Calculate cost from tokens and pricing.
/// Mirrors usageRepo.calculateCost: resolve pricing for (provider, model), then
/// delegate the math to the single source of truth (open-sse/providers/pricing.js).
fn calculate_cost(db: &Db, provider: Option<&str>, model: Option<&str>, tokens: Option<&Value>) -> f64 {
    let (Some(provider), Some(model), Some(tokens)) = (provider, model, tokens) else {
        return 0.0;
    };
    if provider.is_empty() || model.is_empty() {
        return 0.0;
    }
    // Fail-open, exactly like the JS try/catch around this call.
    let pricing = match crate::repos::pricing::get_pricing_for_model(db, provider, model) {
        Ok(Some(p)) => p,
        Ok(None) => return 0.0,
        Err(e) => {
            tracing::error!(error = %e, "Error calculating cost");
            return 0.0;
        }
    };
    nr_data::pricing::calculate_cost_from_tokens(Some(tokens), Some(&pricing))
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
    let cost = calculate_cost(db, provider, model, Some(tokens));

    // Transaction: check for duplicate, insert, update daily, increment lifetime counter
    let inserted = db.transaction(|db| -> Result<bool> {
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
            return Ok(false);
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
            // JS binds only the value; the key is a SQL literal here.
            [next.to_string()],
        )?;

        // Note: stats event emission would go to dashboard via broadcast channel
        Ok(true)
    })?;

    if inserted {
        // Mirrors saveRequestUsage: one optimistically-applied row plus a
        // coalesced update so subscribers re-read period totals.
        stats_emitter().emit_live(&json!({
            "timestamp": timestamp,
            "provider": provider,
            "model": model,
            "connectionId": connection_id,
            "promptTokens": prompt_tokens,
            "completionTokens": completion_tokens,
            "cachedTokens": cached_tokens,
            "cost": cost,
            "status": status,
        }));
        schedule_stats_event("update", 250);
    }

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
            // JS: new Date().setHours(0,0,0,0) then toISOString() - local
            // midnight expressed in UTC. Interpret the local day start against
            // the process timezone, rather than from_naive_utc_and_offset()
            // which would misread local midnight as UTC and shift it.
            let now = chrono::Local::now();
            let start_of_day = now
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .unwrap_or_default();
            start_of_day
                .and_local_timezone(now.timezone())
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .earliest()
                .unwrap_or_else(|| chrono::Utc::now())
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

/// Realtime stats emitter, port of usageRepo.js `statsEmitter`.
///
/// JS exposes a Node EventEmitter with three events (`update`, `pending`,
/// `live`); the usage SSE stream subscribes to all three. Tokio's broadcast
/// channel carries the same three payloads, so every subscriber sees every
/// event. Emissions are debounce-coalesced like scheduleStatsEvent().
pub struct StatsEmitter {
    update: tokio::sync::broadcast::Sender<Value>,
    pending: tokio::sync::broadcast::Sender<Value>,
    live: tokio::sync::broadcast::Sender<Value>,
}

pub static STATS_EMITTER: std::sync::LazyLock<StatsEmitter> = std::sync::LazyLock::new(|| {
    StatsEmitter {
        update: tokio::sync::broadcast::channel(64).0,
        pending: tokio::sync::broadcast::channel(64).0,
        live: tokio::sync::broadcast::channel(64).0,
    }
});

impl StatsEmitter {
    /// `statsEmitter.on("update", fn)` -> a receiver for `update` payloads.
    pub fn subscribe_update(&self) -> tokio::sync::broadcast::Receiver<Value> {
        self.update.subscribe()
    }

    /// `statsEmitter.on("pending", fn)` -> a receiver for `pending` payloads.
    pub fn subscribe_pending(&self) -> tokio::sync::broadcast::Receiver<Value> {
        self.pending.subscribe()
    }

    /// `statsEmitter.on("live", fn)` -> a receiver for `live` row payloads.
    pub fn subscribe_live(&self) -> tokio::sync::broadcast::Receiver<Value> {
        self.live.subscribe()
    }

    fn emit(&self, tx: &tokio::sync::broadcast::Sender<Value>, event: &str, payload: Value) {
        // No receivers yet is normal (the stream may not be connected); JS
        // EventEmitter.emit on zero listeners is the same no-op.
        if let Err(e) = tx.send(payload) {
            tracing::trace!(event, error = %e, "stats emitter: no active subscribers");
        }
    }

    /// Emit `update`: period totals changed, so the summary should be re-read.
    pub fn emit_update(&self, period: &str) {
        self.emit(&self.update, "update", json!({ "period": period }));
    }

    /// Emit `pending`: active/recent request lists changed.
    pub fn emit_pending(&self) {
        self.emit(&self.pending, "pending", json!({}));
    }

    /// Emit `live`: one just-written usage row, applied optimistically by clients.
    pub fn emit_live(&self, row: &Value) {
        self.emit(&self.live, "live", row.clone());
    }
}

/// Schedule a stats event after a short delay, coalescing repeats, exactly like
/// scheduleStatsEvent(): `update` for a new usage row, `pending` for tracking.
/// Each event type carries at most one timer.
static STATS_EMIT_TIMERS: std::sync::LazyLock<Mutex<HashMap<&'static str, bool>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn schedule_stats_event(event: &str, delay_ms: u64) {
    let mut timers = STATS_EMIT_TIMERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let key: &'static str = if event == "update" { "update" } else { "pending" };
    if *timers.get(key).unwrap_or(&false) {
        return;
    }
    timers.insert(key, true);
    drop(timers);

    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        STATS_EMIT_TIMERS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key, false);
        // Coalesced fires don't know the period that triggered them; the
        // subscriber re-reads from the DB for its own selected period anyway.
        if key == "update" {
            stats_emitter().emit_update("today");
        } else {
            stats_emitter().emit_pending();
        }
    });
}

/// Get the process-wide stats emitter (JS `statsEmitter`).
pub fn stats_emitter() -> &'static StatsEmitter {
    &STATS_EMITTER
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

    #[tokio::test]
    async fn save_request_usage_roundtrip() {
        let (_dir, db) = db();
        let entry = json!({
            "timestamp": "2026-01-01T00:00:00.000Z",
            "provider": "acme",
            "model": "acme-1",
            "connectionId": "conn-1",
            "apiKey": "key-1",
            "endpoint": "chat",
            "status": "ok",
            "tokens": {"prompt_tokens": 1000, "completion_tokens": 500},
        });
        save_request_usage(&db, &entry).await.unwrap();

        let history = get_usage_history(&db, None).unwrap();
        assert_eq!(history.len(), 1);
        let row = &history[0];
        // get_usage_history's projection (matching JS) has no promptTokens column.
        assert_eq!(row["provider"], json!("acme"));
        assert_eq!(row["model"], json!("acme-1"));
        assert_eq!(row["status"], json!("ok"));
        assert_eq!(row["tokens"]["prompt_tokens"], json!(1000));
        assert_eq!(row["tokens"]["completion_tokens"], json!(500));

        // Daily aggregate was upserted. Read it back directly: the bucket date is
        // the server's local date, which differs from UTC in the test env.
        let day_count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM usageDaily",
                &[],
                |row| row.get(0),
            )
            .unwrap()
            .unwrap_or(0);
        assert_eq!(day_count, 1);
        let day: Value = serde_json::from_str(
            &db.query_row(
                "SELECT data FROM usageDaily",
                &[],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
            .unwrap(),
        )
        .unwrap();
        assert_eq!(day["requests"], json!(1));
        assert_eq!(day["promptTokens"], json!(1000));

        // Lifetime counter incremented.
        let lifetime = db
            .query_row(
                "SELECT value FROM _meta WHERE key = 'totalRequestsLifetime'",
                &[],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
            .unwrap();
        assert_eq!(lifetime, "1");

        // Duplicate timestamp/provider/model/tokens is a no-op (no double count).
        save_request_usage(&db, &entry).await.unwrap();
        let history = get_usage_history(&db, None).unwrap();
        assert_eq!(history.len(), 1);
        let lifetime = db
            .query_row(
                "SELECT value FROM _meta WHERE key = 'totalRequestsLifetime'",
                &[],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
            .unwrap();
        assert_eq!(lifetime, "1");
    }

    #[tokio::test]
    async fn realtime_summary_loads_saved_rows() {
        let (_dir, db) = db();
        // Use a "now" timestamp so the row lands inside the today/24h windows.
        let entry = json!({
            "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "provider": "acme",
            "model": "acme-1",
            "tokens": {"prompt_tokens": 300, "completion_tokens": 200},
        });
        save_request_usage(&db, &entry).await.unwrap();

        let summary = get_realtime_summary(&db, Some("today")).unwrap();
        assert_eq!(summary["totalRequests"], json!(1));
        assert_eq!(summary["totalPromptTokens"], json!(300));
        assert_eq!(summary["totalCompletionTokens"], json!(200));

        let active = get_active_requests(&db).unwrap();
        // by_account only counts in-flight (started) requests; this test never
        // starts one, so none of its recent rows should be active. (Other tests
        // share the process-wide pending map, so filter by our row's model.)
        let active_for_model = active["activeRequests"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["model"] == json!("acme-1"));
        assert!(!active_for_model);
        // recentRequests come from usageHistory, which is per-Db (temp dir).
        let recent = active["recentRequests"].as_array().unwrap();
        assert!(recent.iter().any(|r| r["model"] == json!("acme-1")));
    }

    #[tokio::test]
    async fn calculate_cost_resolves_pricing() {
        let (_dir, db) = db();
        // acme-1 has no default pricing; verify cost is 0 for unlisted models.
        let cost = calculate_cost(
            &db,
            Some("acme"),
            Some("acme-1"),
            Some(&json!({"prompt_tokens": 1000, "completion_tokens": 0})),
        );
        assert_eq!(cost, 0.0);

        // A priced model: 1M input tokens at $1/M = $1.00.
        crate::repos::pricing::update_pricing(
            &db,
            &json!({"acme": {"acme-1": {"input": 1.0, "output": 2.0}}}),
        )
        .unwrap();
        let cost = calculate_cost(
            &db,
            Some("acme"),
            Some("acme-1"),
            Some(&json!({"prompt_tokens": 1_000_000, "completion_tokens": 0})),
        );
        assert!((cost - 1.0).abs() < 1e-9);

        // Missing inputs are fail-open (0.0), never an error.
        assert_eq!(calculate_cost(&db, None, Some("m"), Some(&json!({}))), 0.0);
        assert_eq!(calculate_cost(&db, Some("p"), None, Some(&json!({}))), 0.0);
        assert_eq!(calculate_cost(&db, Some("p"), Some("m"), None), 0.0);
    }

    #[tokio::test]
    async fn track_pending_roundtrip_with_connection_names() {
        let (_dir, db) = db();
        // create_provider_connection assigns its own id (dedup rules in JS parity).
        let conn = crate::repos::connections::create_provider_connection(
            &db,
            &json!({"provider": "openai", "name": "My Account", "authType": "api-key", "apiKey": "k"}),
        )
        .unwrap();
        let conn_id = conn["id"].as_str().unwrap().to_string();

        track_pending_request(&db, "gpt-5", "openai", Some(&conn_id), true, false).unwrap();
        let active = get_active_requests(&db).unwrap();
        let list = active["activeRequests"].as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["model"], json!("gpt-5"));
        assert_eq!(list[0]["provider"], json!("openai"));
        assert_eq!(list[0]["account"], json!("My Account"));
        assert_eq!(list[0]["count"], json!(1));

        // Second start for the same key increments.
        track_pending_request(&db, "gpt-5", "openai", Some(&conn_id), true, false).unwrap();
        let active = get_active_requests(&db).unwrap();
        assert_eq!(active["activeRequests"][0]["count"], json!(2));

        // Finish one -> decrements; finish the rest -> entry disappears.
        track_pending_request(&db, "gpt-5", "openai", Some(&conn_id), false, false).unwrap();
        track_pending_request(&db, "gpt-5", "openai", Some(&conn_id), false, false).unwrap();
        let active = get_active_requests(&db).unwrap();
        assert!(active["activeRequests"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn track_pending_records_error_provider_for_10s() {
        let (_dir, db) = db();
        track_pending_request(&db, "m", "openai", Some("conn-9"), true, false).unwrap();
        track_pending_request(&db, "m", "openai", Some("conn-9"), false, true).unwrap();

        let active = get_active_requests(&db).unwrap();
        assert_eq!(active["errorProvider"], json!("openai"));
    }

    #[test]
    fn stats_emitter_broadcasts_update_and_live() {
        let mut update_rx = stats_emitter().subscribe_update();
        let mut live_rx = stats_emitter().subscribe_live();

        stats_emitter().emit_update("today");
        stats_emitter().emit_live(&json!({"model": "gpt-5", "promptTokens": 1}));

        // broadcast delivers to every subscriber.
        let update = update_rx.blocking_recv().unwrap();
        assert_eq!(update["period"], json!("today"));
        let live = live_rx.blocking_recv().unwrap();
        assert_eq!(live["model"], json!("gpt-5"));
    }

    #[tokio::test]
    async fn pending_track_schedules_emit_without_panicking_outside_runtime() {
        // schedule_stats_event only spawns a task; even with no runtime attached
        // (or a fresh one) the synchronous state updates still work.
        let (_dir, db) = db();
        track_pending_request(&db, "m", "openai", Some("conn-x"), true, false).unwrap();
        let active = get_active_requests(&db).unwrap();
        assert!(!active["activeRequests"].as_array().unwrap().is_empty());
        track_pending_request(&db, "m", "openai", Some("conn-x"), false, false).unwrap();
    }
}
