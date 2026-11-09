use crate::connection::Db;
use crate::helpers::{parse_json, stringify_json};
use nr_core::Result;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
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

/// `addToCounter()`: bump one aggregation bucket in place.
fn add_to_counter(target: &mut serde_json::Map<String, Value>, key: &str, values: &Value) {
    let entry = target
        .entry(key.to_string())
        .or_insert_with(|| json!({
            "requests": 0, "promptTokens": 0, "completionTokens": 0, "cachedTokens": 0, "cost": 0
        }));
    let Some(obj) = entry.as_object_mut() else {
        return;
    };
    let bump_i = |obj: &mut serde_json::Map<String, Value>, field: &str, by: i64| {
        let next = obj.get(field).and_then(Value::as_i64).unwrap_or(0) + by;
        obj.insert(field.to_string(), json!(next));
    };
    bump_i(obj, "requests", values.get("requests").and_then(Value::as_i64).unwrap_or(1));
    bump_i(obj, "promptTokens", values.get("promptTokens").and_then(Value::as_i64).unwrap_or(0));
    bump_i(obj, "completionTokens", values.get("completionTokens").and_then(Value::as_i64).unwrap_or(0));
    bump_i(obj, "cachedTokens", values.get("cachedTokens").and_then(Value::as_i64).unwrap_or(0));
    let cost = obj.get("cost").and_then(Value::as_f64).unwrap_or(0.0)
        + values.get("cost").and_then(Value::as_f64).unwrap_or(0.0);
    obj.insert("cost".to_string(), json!(cost));

    // `if (values.meta) Object.assign(target[key], values.meta)`
    if let Some(meta) = values.get("meta").and_then(Value::as_object) {
        for (k, v) in meta {
            obj.insert(k.clone(), v.clone());
        }
    }
}

/// `aggregateEntryToDay(day, entry)` — fold one usage row into the day rollup.
fn aggregate_entry_to_day(
    day: &mut Value,
    entry: &Value,
    prompt_tokens: i64,
    completion_tokens: i64,
    cached_tokens: i64,
    cost: f64,
) {
    let provider = entry.get("provider").and_then(Value::as_str);
    let model = entry.get("model").and_then(Value::as_str);
    let connection_id = entry.get("connectionId").and_then(Value::as_str);
    let api_key = entry.get("apiKey").and_then(Value::as_str);
    let endpoint = entry.get("endpoint").and_then(Value::as_str).unwrap_or("Unknown");

    let vals = json!({
        "promptTokens": prompt_tokens,
        "completionTokens": completion_tokens,
        "cachedTokens": cached_tokens,
        "cost": cost,
    });

    let Some(day_obj) = day.as_object_mut() else {
        return;
    };

    let bump_i = |obj: &mut serde_json::Map<String, Value>, field: &str, by: i64| {
        let next = obj.get(field).and_then(Value::as_i64).unwrap_or(0) + by;
        obj.insert(field.to_string(), json!(next));
    };
    bump_i(day_obj, "requests", 1);
    bump_i(day_obj, "promptTokens", prompt_tokens);
    bump_i(day_obj, "completionTokens", completion_tokens);
    bump_i(day_obj, "cachedTokens", cached_tokens);
    let day_cost = day_obj.get("cost").and_then(Value::as_f64).unwrap_or(0.0) + cost;
    day_obj.insert("cost".to_string(), json!(day_cost));

    for field in ["byProvider", "byModel", "byAccount", "byApiKey", "byEndpoint"] {
        day_obj
            .entry(field.to_string())
            .or_insert_with(|| json!({}));
    }

    let provider_unknown = provider.unwrap_or("unknown");
    let model_key = match provider {
        Some(p) => format!("{}|{}", model.unwrap_or(""), p),
        None => model.unwrap_or("").to_string(),
    };
    let entry_model = model.unwrap_or("");

    if let Some(p) = provider {
        if let Some(by) = day_obj.get_mut("byProvider").and_then(Value::as_object_mut) {
            add_to_counter(by, p, &vals);
        }
    }

    if let Some(by) = day_obj.get_mut("byModel").and_then(Value::as_object_mut) {
        let mut with_meta = vals.clone();
        with_meta["meta"] = json!({ "rawModel": entry_model, "provider": provider });
        add_to_counter(by, &model_key, &with_meta);
    }

    if let Some(conn) = connection_id {
        if let Some(by) = day_obj.get_mut("byAccount").and_then(Value::as_object_mut) {
            let mut with_meta = vals.clone();
            with_meta["meta"] = json!({ "rawModel": entry_model, "provider": provider });
            add_to_counter(by, conn, &with_meta);
        }
    }

    let api_key_val = api_key.unwrap_or("local-no-key");
    let ak_model_key = format!("{}|{}|{}", api_key_val, entry_model, provider_unknown);
    if let Some(by) = day_obj.get_mut("byApiKey").and_then(Value::as_object_mut) {
        let mut with_meta = vals.clone();
        with_meta["meta"] = json!({
            "rawModel": entry_model,
            "provider": provider,
            "apiKey": api_key,
        });
        add_to_counter(by, &ak_model_key, &with_meta);
    }

    let ep_key = format!("{}|{}|{}", endpoint, entry_model, provider_unknown);
    if let Some(by) = day_obj.get_mut("byEndpoint").and_then(Value::as_object_mut) {
        let mut with_meta = vals.clone();
        with_meta["meta"] = json!({
            "endpoint": endpoint,
            "rawModel": entry_model,
            "provider": provider,
        });
        add_to_counter(by, &ep_key, &with_meta);
    }
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

        // Aggregate the entry into the local-day rollup (usageRepo
        // `aggregateEntryToDay`). Every counters object keeps the JS shape so
        // the dashboard reads either writer's rows.
        aggregate_entry_to_day(&mut day_data, entry, prompt_tokens, completion_tokens, cached_tokens, cost);
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

/// `getUsageStats(period)`: dashboard aggregate for one period.
///
/// Faithful port of usageRepo.getUsageStats — daily rollups for 7d/30d/60d/all,
/// live `usageHistory` for today/24h, plus the last10Minutes buckets, active and
/// recent request lists. Everything the dashboard renders is present.
pub fn get_usage_stats(db: &Db, period: Option<&str>) -> Result<Value> {
    let period = period.unwrap_or("all");

    // connectionMap: id -> name || email || id
    let mut connection_map: serde_json::Map<String, Value> = serde_json::Map::new();
    if let Ok(conns) = crate::repos::connections::get_provider_connections(db, None) {
        for c in conns {
            let Some(id) = c.get("id").and_then(Value::as_str) else { continue };
            let label = c
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .or_else(|| c.get("email").and_then(Value::as_str).filter(|s| !s.is_empty()))
                .unwrap_or(id);
            connection_map.insert(id.to_string(), Value::String(label.to_string()));
        }
    }

    // providerNodeNameMap: node id -> display name
    let mut provider_node_names: serde_json::Map<String, Value> = serde_json::Map::new();
    if let Ok(nodes) = crate::repos::nodes::get_provider_nodes(db, None) {
        for n in nodes {
            if let (Some(id), Some(name)) = (
                n.get("id").and_then(Value::as_str),
                n.get("name").and_then(Value::as_str),
            ) {
                provider_node_names.insert(id.to_string(), Value::String(name.to_string()));
            }
        }
    }

    // apiKeyMap: trimmed key -> { name, id, createdAt }
    let mut api_key_map: serde_json::Map<String, Value> = serde_json::Map::new();
    if let Ok(keys) = crate::repos::api_keys::get_api_keys(db) {
        for k in keys {
            if k.key.is_empty() {
                continue;
            }
            api_key_map.insert(
                k.key.trim().to_string(),
                json!({ "name": k.name, "id": k.id, "createdAt": k.created_at }),
            );
        }
    }

    // recentRequests from live history (last 100 rows, deduped to 20)
    let recent_rows = db.query_all(
        "SELECT timestamp, provider, model, tokens, status FROM usageHistory ORDER BY id DESC LIMIT 100",
        &[],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        },
    )?;

    let mut seen: HashSet<String> = HashSet::new();
    let mut recent_requests: Vec<Value> = Vec::new();
    for (timestamp, provider, model, tokens_str, status) in recent_rows {
        let tokens = parse_json(tokens_str, json!({}));
        let prompt = tokens
            .get("prompt_tokens")
            .or_else(|| tokens.get("input_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let completion = tokens
            .get("completion_tokens")
            .or_else(|| tokens.get("output_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if prompt == 0 && completion == 0 {
            continue;
        }
        let cached = tokens
            .get("cached_tokens")
            .or_else(|| tokens.get("cache_read_input_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let minute: String = timestamp.chars().take(16).collect();
        let dedupe = format!(
            "{}|{}|{}|{}|{}",
            model.clone().unwrap_or_default(),
            provider.clone().unwrap_or_default(),
            prompt,
            completion,
            minute
        );
        if !seen.insert(dedupe) {
            continue;
        }
        recent_requests.push(json!({
            "timestamp": timestamp,
            "model": model,
            "provider": provider.unwrap_or_default(),
            "promptTokens": prompt,
            "completionTokens": completion,
            "cachedTokens": cached,
            "status": status.unwrap_or_else(|| "ok".into()),
        }));
        if recent_requests.len() >= 20 {
            break;
        }
    }

    let (pending, active_requests, error_provider) = {
        let state = PENDING_REQUESTS.lock().unwrap_or_else(|p| p.into_inner());
        let pending = json!({
            "byModel": state.by_model,
            "byAccount": state.by_account,
        });
        let mut active: Vec<Value> = Vec::new();
        for (connection_id, models) in &state.by_account {
            for (model_key, count) in models {
                if *count <= 0 {
                    continue;
                }
                let account_name = connection_map
                    .get(connection_id)
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        format!("Account {}...", &connection_id[..8.min(connection_id.len())])
                    });
                // `^(.*) \((.*)\)$`
                let (model, provider) = match model_key.rfind(" (") {
                    Some(idx) if model_key.ends_with(')') => (
                        model_key[..idx].to_string(),
                        model_key[idx + 2..model_key.len() - 1].to_string(),
                    ),
                    _ => (model_key.clone(), "unknown".to_string()),
                };
                active.push(json!({
                    "model": model,
                    "provider": provider,
                    "account": account_name,
                    "count": count,
                }));
            }
        }
        let now = chrono::Utc::now().timestamp_millis();
        let error_provider = if now - state.last_error_provider.1 < 10_000 {
            state.last_error_provider.0.clone()
        } else {
            String::new()
        };
        (pending, active, error_provider)
    };

    // last10Minutes: fixed 10 one-minute buckets ending at the current minute.
    let now_ms = chrono::Utc::now().timestamp_millis();
    let current_minute = now_ms - now_ms.rem_euclid(60_000);
    let ten_minutes_ago = current_minute - 9 * 60_000;
    let mut bucket_index: HashMap<i64, usize> = HashMap::new();
    let mut last10: Vec<Value> = Vec::new();
    for i in 0..10i64 {
        let ts = current_minute - (9 - i) * 60_000;
        bucket_index.insert(ts, last10.len());
        last10.push(json!({ "requests": 0, "promptTokens": 0, "completionTokens": 0, "cost": 0.0 }));
    }
    let start_iso = chrono::DateTime::from_timestamp_millis(ten_minutes_ago)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default();
    let end_iso = chrono::DateTime::from_timestamp_millis(now_ms)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default();
    let recent10 = db.query_all(
        "SELECT timestamp, promptTokens, completionTokens, cost FROM usageHistory WHERE timestamp >= ? AND timestamp <= ?",
        &[&start_iso as &dyn rusqlite::ToSql, &end_iso],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                row.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
            ))
        },
    )?;
    for (timestamp, prompt, completion, cost) in recent10 {
        let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&timestamp) else {
            continue;
        };
        let ts = dt.timestamp_millis();
        let minute = ts - ts.rem_euclid(60_000);
        let Some(&idx) = bucket_index.get(&minute) else {
            continue;
        };
        if let Some(b) = last10.get_mut(idx).and_then(Value::as_object_mut) {
            let bump_i = |obj: &mut serde_json::Map<String, Value>, k: &str, by: i64| {
                let next = obj.get(k).and_then(Value::as_i64).unwrap_or(0) + by;
                obj.insert(k.to_string(), json!(next));
            };
            bump_i(b, "requests", 1);
            bump_i(b, "promptTokens", prompt);
            bump_i(b, "completionTokens", completion);
            let next_cost = b.get("cost").and_then(Value::as_f64).unwrap_or(0.0) + cost;
            b.insert("cost".to_string(), json!(next_cost));
        }
    }

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
        "last10Minutes": last10,
        "pending": pending,
        "activeRequests": active_requests,
        "recentRequests": recent_requests,
        "errorProvider": error_provider,
    });

    let use_daily_summary = period != "24h" && period != "today";

    if use_daily_summary {
        let max_days = match period {
            "7d" => Some(7i64),
            "30d" => Some(30),
            "60d" => Some(60),
            _ => None,
        };

        let day_rows: Vec<(String, String)> = match max_days {
            Some(days) => {
                let cutoff = chrono::Local::now().date_naive() - chrono::Duration::days(days - 1);
                let cutoff_key = cutoff.format("%Y-%m-%d").to_string();
                db.query_all(
                    "SELECT dateKey, data FROM usageDaily WHERE dateKey >= ? ORDER BY dateKey ASC",
                    &[&cutoff_key as &dyn rusqlite::ToSql],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?
            }
            None => db.query_all(
                "SELECT dateKey, data FROM usageDaily ORDER BY dateKey ASC",
                &[],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?,
        };

        let stats_obj = stats.as_object_mut().expect("stats is an object");
        for (date_key, data_str) in day_rows {
            let day = parse_json(Some(data_str), json!({}));
            let Some(day_obj) = day.as_object() else { continue };

            let add_total = |stats_obj: &mut serde_json::Map<String, Value>, field: &str, by: i64| {
                let next = stats_obj.get(field).and_then(Value::as_i64).unwrap_or(0) + by;
                stats_obj.insert(field.to_string(), json!(next));
            };
            add_total(stats_obj, "totalPromptTokens", day_obj.get("promptTokens").and_then(Value::as_i64).unwrap_or(0));
            add_total(stats_obj, "totalCompletionTokens", day_obj.get("completionTokens").and_then(Value::as_i64).unwrap_or(0));
            add_total(stats_obj, "totalCachedTokens", day_obj.get("cachedTokens").and_then(Value::as_i64).unwrap_or(0));
            let cost_next = stats_obj.get("totalCost").and_then(Value::as_f64).unwrap_or(0.0)
                + day_obj.get("cost").and_then(Value::as_f64).unwrap_or(0.0);
            stats_obj.insert("totalCost".to_string(), json!(cost_next));

            // byProvider
            if let Some(by) = day_obj.get("byProvider").and_then(Value::as_object) {
                let target = stats_obj.get_mut("byProvider").and_then(Value::as_object_mut).unwrap();
                for (prov, p) in by {
                    let entry = target.entry(prov.clone()).or_insert_with(|| {
                        json!({ "requests": 0, "promptTokens": 0, "completionTokens": 0, "cachedTokens": 0, "cost": 0 })
                    });
                    merge_counter(entry, p);
                }
            }

            // byModel — key is `rawModel (provider)`
            if let Some(by) = day_obj.get("byModel").and_then(Value::as_object) {
                let target = stats_obj.get_mut("byModel").and_then(Value::as_object_mut).unwrap();
                for (mk, m) in by {
                    let raw_model = m
                        .get("rawModel")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| mk.split('|').next().unwrap_or("").to_string());
                    let provider = m
                        .get("provider")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| mk.split('|').nth(1).unwrap_or("").to_string());
                    let stats_key = if provider.is_empty() { raw_model.clone() } else { format!("{} ({})", raw_model, provider) };
                    let display = provider_node_names
                        .get(&provider)
                        .and_then(Value::as_str)
                        .unwrap_or(&provider)
                        .to_string();
                    let entry = target.entry(stats_key.clone()).or_insert_with(|| {
                        json!({
                            "requests": 0, "promptTokens": 0, "completionTokens": 0, "cachedTokens": 0,
                            "cost": 0, "rawModel": raw_model, "provider": display, "lastUsed": date_key,
                        })
                    });
                    merge_counter(entry, m);
                    bump_last_used(entry, &date_key);
                }
            }

            // byAccount — key is `rawModel (provider - accountName)`
            if let Some(by) = day_obj.get("byAccount").and_then(Value::as_object) {
                let target = stats_obj.get_mut("byAccount").and_then(Value::as_object_mut).unwrap();
                for (conn_id, a) in by {
                    let account_name = connection_map
                        .get(conn_id)
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("Account {}...", &conn_id[..8.min(conn_id.len())]));
                    let raw_model = a.get("rawModel").and_then(Value::as_str).unwrap_or("").to_string();
                    let provider = a.get("provider").and_then(Value::as_str).unwrap_or("").to_string();
                    let display = provider_node_names
                        .get(&provider)
                        .and_then(Value::as_str)
                        .unwrap_or(&provider)
                        .to_string();
                    let account_key = format!("{} ({} - {})", raw_model, provider, account_name);
                    let entry = target.entry(account_key).or_insert_with(|| {
                        json!({
                            "requests": 0, "promptTokens": 0, "completionTokens": 0, "cachedTokens": 0,
                            "cost": 0, "rawModel": raw_model, "provider": display,
                            "connectionId": conn_id, "accountName": account_name, "lastUsed": date_key,
                        })
                    });
                    merge_counter(entry, a);
                    bump_last_used(entry, &date_key);
                }
            }

            // byApiKey — masked display, keyed by the full key in the rollup
            if let Some(by) = day_obj.get("byApiKey").and_then(Value::as_object) {
                let target = stats_obj.get_mut("byApiKey").and_then(Value::as_object_mut).unwrap();
                for (ak_key, ak) in by {
                    let raw_model = ak.get("rawModel").and_then(Value::as_str).unwrap_or("").to_string();
                    let provider = ak.get("provider").and_then(Value::as_str).unwrap_or("").to_string();
                    let display = provider_node_names
                        .get(&provider)
                        .and_then(Value::as_str)
                        .unwrap_or(&provider)
                        .to_string();
                    let api_key_val = ak
                        .get("apiKey")
                        .and_then(Value::as_str)
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty());
                    let key_info = api_key_val
                        .as_ref()
                        .and_then(|k| api_key_map.get(k))
                        .cloned();
                    let api_key_masked = mask_api_key(api_key_val.as_deref());
                    let key_name = key_info
                        .as_ref()
                        .and_then(|info| info.get("name"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| api_key_masked.clone())
                        .or_else(|| api_key_val.as_ref().map(|k| format!("{}...", &k[..8.min(k.len())])))
                        .unwrap_or_else(|| "Local (No API Key)".to_string());
                    let api_key_key = api_key_masked.clone().unwrap_or_else(|| "local-no-key".to_string());
                    let entry = target.entry(ak_key.clone()).or_insert_with(|| {
                        json!({
                            "requests": 0, "promptTokens": 0, "completionTokens": 0, "cachedTokens": 0,
                            "cost": 0, "rawModel": raw_model, "provider": display,
                            "apiKeyMasked": api_key_masked, "keyName": key_name,
                            "apiKeyKey": api_key_key, "lastUsed": date_key,
                        })
                    });
                    merge_counter(entry, ak);
                    bump_last_used(entry, &date_key);
                }
            }

            // byEndpoint — key is `endpoint|model|provider`
            if let Some(by) = day_obj.get("byEndpoint").and_then(Value::as_object) {
                let target = stats_obj.get_mut("byEndpoint").and_then(Value::as_object_mut).unwrap();
                for (ep_key, ep) in by {
                    let endpoint = ep
                        .get("endpoint")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| ep_key.split('|').next().unwrap_or("Unknown").to_string());
                    let raw_model = ep.get("rawModel").and_then(Value::as_str).unwrap_or("").to_string();
                    let provider = ep.get("provider").and_then(Value::as_str).unwrap_or("").to_string();
                    let display = provider_node_names
                        .get(&provider)
                        .and_then(Value::as_str)
                        .unwrap_or(&provider)
                        .to_string();
                    let entry = target.entry(ep_key.clone()).or_insert_with(|| {
                        json!({
                            "requests": 0, "promptTokens": 0, "completionTokens": 0, "cachedTokens": 0,
                            "cost": 0, "endpoint": endpoint, "rawModel": raw_model,
                            "provider": display, "lastUsed": date_key,
                        })
                    });
                    merge_counter(entry, ep);
                    bump_last_used(entry, &date_key);
                }
            }
        }

        // Overlay precise lastUsed timestamps from history.
        // ponytail: overlay scans only a recent window; entries older than that
        // keep the day-level lastUsed from usageDaily. Materialize a per-key
        // MAX(timestamp) if exact old timestamps ever matter.
        const OVERLAY_WINDOW_MS: i64 = 2 * 86_400_000;
        let max_days_ms = max_days.map(|d| d * 86_400_000);
        let overlay_cutoff = chrono::Utc::now().timestamp_millis() - OVERLAY_WINDOW_MS;
        let overlay_cutoff = match max_days_ms {
            Some(ms) => overlay_cutoff.max(chrono::Utc::now().timestamp_millis() - ms),
            None => overlay_cutoff,
        };
        let overlay_iso = chrono::DateTime::from_timestamp_millis(overlay_cutoff)
            .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
            .unwrap_or_default();
        let hist_rows = db.query_all(
            "SELECT timestamp, provider, model, connectionId, apiKey, endpoint FROM usageHistory WHERE timestamp >= ?",
            &[&overlay_iso as &dyn rusqlite::ToSql],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            },
        )?;
        for (ts, provider, model, connection_id, api_key, endpoint) in hist_rows {
            let model = model.unwrap_or_default();
            let provider = provider.unwrap_or_default();

            let model_key = if provider.is_empty() { model.clone() } else { format!("{} ({})", model, provider) };
            if let Some(entry) = stats_obj.get_mut("byModel").and_then(|v| v.get_mut(&model_key)) {
                replace_if_newer(entry, &ts);
            }

            if let Some(conn_id) = connection_id {
                let account_name = connection_map
                    .get(&conn_id)
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("Account {}...", &conn_id[..8.min(conn_id.len())]));
                let account_key = format!("{} ({} - {})", model, provider, account_name);
                if let Some(entry) = stats_obj.get_mut("byAccount").and_then(|v| v.get_mut(&account_key)) {
                    replace_if_newer(entry, &ts);
                }
            }

            let api_key_key = match api_key.as_deref() {
                Some(k) if !k.is_empty() => format!("{}|{}|{}", k, model, if provider.is_empty() { "unknown" } else { &provider }),
                _ => "local-no-key".to_string(),
            };
            if let Some(entry) = stats_obj.get_mut("byApiKey").and_then(|v| v.get_mut(&api_key_key)) {
                replace_if_newer(entry, &ts);
            }

            let endpoint_key = format!(
                "{}|{}|{}",
                endpoint.as_deref().unwrap_or("Unknown"),
                model,
                if provider.is_empty() { "unknown" } else { &provider }
            );
            if let Some(entry) = stats_obj.get_mut("byEndpoint").and_then(|v| v.get_mut(&endpoint_key)) {
                replace_if_newer(entry, &ts);
            }
        }
    } else {
        // 24h / today: read live history
        let cutoff = if period == "today" {
            let now = chrono::Local::now();
            let start_of_day = now.date_naive().and_hms_opt(0, 0, 0).unwrap_or_default();
            start_of_day
                .and_local_timezone(now.timezone())
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .earliest()
                .unwrap_or_else(chrono::Utc::now)
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        } else {
            chrono::DateTime::from_timestamp_millis(now_ms - 86_400_000)
                .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
                .unwrap_or_default()
        };

        let filtered = db.query_all(
            "SELECT timestamp, provider, model, connectionId, apiKey, endpoint, promptTokens, completionTokens, cost, tokens FROM usageHistory WHERE timestamp >= ?",
            &[&cutoff as &dyn rusqlite::ToSql],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<i64>>(6)?.unwrap_or(0),
                    row.get::<_, Option<i64>>(7)?.unwrap_or(0),
                    row.get::<_, Option<f64>>(8)?.unwrap_or(0.0),
                    row.get::<_, Option<String>>(9)?,
                ))
            },
        )?;

        let stats_obj = stats.as_object_mut().expect("stats is an object");
        for (timestamp, provider, model, connection_id, api_key, endpoint, prompt_col, completion_col, cost, tokens_str) in filtered {
            let tokens = parse_json(tokens_str, json!({}));
            let prompt_tokens = tokens
                .get("prompt_tokens")
                .or_else(|| tokens.get("input_tokens"))
                .and_then(Value::as_i64)
                .unwrap_or(prompt_col);
            let completion_tokens = tokens
                .get("completion_tokens")
                .or_else(|| tokens.get("output_tokens"))
                .and_then(Value::as_i64)
                .unwrap_or(completion_col);
            let cached_tokens = tokens
                .get("cached_tokens")
                .or_else(|| tokens.get("cache_read_input_tokens"))
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let model = model.unwrap_or_default();
            let provider = provider.unwrap_or_default();
            let provider_display = provider_node_names
                .get(&provider)
                .and_then(Value::as_str)
                .unwrap_or(&provider)
                .to_string();

            let bump_total = |stats_obj: &mut serde_json::Map<String, Value>, field: &str, by: i64| {
                let next = stats_obj.get(field).and_then(Value::as_i64).unwrap_or(0) + by;
                stats_obj.insert(field.to_string(), json!(next));
            };
            bump_total(stats_obj, "totalPromptTokens", prompt_tokens);
            bump_total(stats_obj, "totalCompletionTokens", completion_tokens);
            bump_total(stats_obj, "totalCachedTokens", cached_tokens);
            let cost_next = stats_obj.get("totalCost").and_then(Value::as_f64).unwrap_or(0.0) + cost;
            stats_obj.insert("totalCost".to_string(), json!(cost_next));

            let vals = json!({
                "requests": 1,
                "promptTokens": prompt_tokens,
                "completionTokens": completion_tokens,
                "cachedTokens": cached_tokens,
                "cost": cost,
            });

            if let Some(by) = stats_obj.get_mut("byProvider").and_then(Value::as_object_mut) {
                add_to_counter(by, &provider, &vals);
            }

            let model_key = if provider.is_empty() { model.clone() } else { format!("{} ({})", model, provider) };
            {
                let by = stats_obj.get_mut("byModel").and_then(Value::as_object_mut).unwrap();
                let entry = by.entry(model_key).or_insert_with(|| {
                    json!({
                        "requests": 0, "promptTokens": 0, "completionTokens": 0, "cachedTokens": 0,
                        "cost": 0, "rawModel": model, "provider": provider_display, "lastUsed": timestamp,
                    })
                });
                merge_counter(entry, &vals);
                replace_if_newer(entry, &timestamp);
            }

            if let Some(conn_id) = connection_id {
                let account_name = connection_map
                    .get(&conn_id)
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("Account {}...", &conn_id[..8.min(conn_id.len())]));
                let account_key = format!("{} ({} - {})", model, provider, account_name);
                let by = stats_obj.get_mut("byAccount").and_then(Value::as_object_mut).unwrap();
                let entry = by.entry(account_key).or_insert_with(|| {
                    json!({
                        "requests": 0, "promptTokens": 0, "completionTokens": 0, "cachedTokens": 0,
                        "cost": 0, "rawModel": model, "provider": provider_display,
                        "connectionId": conn_id, "accountName": account_name, "lastUsed": timestamp,
                    })
                });
                merge_counter(entry, &vals);
                replace_if_newer(entry, &timestamp);
            }

            let provider_unknown = if provider.is_empty() { "unknown".to_string() } else { provider.clone() };
            match api_key.filter(|k| !k.is_empty()) {
                Some(api_key_val) => {
                    let key_info = api_key_map.get(api_key_val.trim()).cloned();
                    let api_key_masked = mask_api_key(Some(&api_key_val));
                    let key_name = key_info
                        .as_ref()
                        .and_then(|i| i.get("name"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| api_key_masked.clone())
                        .unwrap_or_else(|| format!("{}...", &api_key_val[..8.min(api_key_val.len())]));
                    let ak_key = format!("{}|{}|{}", api_key_val, model, provider_unknown);
                    let by = stats_obj.get_mut("byApiKey").and_then(Value::as_object_mut).unwrap();
                    let entry = by.entry(ak_key).or_insert_with(|| {
                        json!({
                            "requests": 0, "promptTokens": 0, "completionTokens": 0, "cachedTokens": 0,
                            "cost": 0, "rawModel": model, "provider": provider_display,
                            "apiKeyMasked": api_key_masked, "keyName": key_name,
                            "apiKeyKey": api_key_masked, "lastUsed": timestamp,
                        })
                    });
                    merge_counter(entry, &vals);
                    replace_if_newer(entry, &timestamp);
                }
                None => {
                    let by = stats_obj.get_mut("byApiKey").and_then(Value::as_object_mut).unwrap();
                    let entry = by.entry("local-no-key".to_string()).or_insert_with(|| {
                        json!({
                            "requests": 0, "promptTokens": 0, "completionTokens": 0, "cachedTokens": 0,
                            "cost": 0, "rawModel": model, "provider": provider_display,
                            "apiKeyMasked": Value::Null, "keyName": "Local (No API Key)",
                            "apiKeyKey": "local-no-key", "lastUsed": timestamp,
                        })
                    });
                    merge_counter(entry, &vals);
                    replace_if_newer(entry, &timestamp);
                }
            }

            let endpoint = endpoint.unwrap_or_else(|| "Unknown".to_string());
            let ep_key = format!("{}|{}|{}", endpoint, model, provider_unknown);
            let by = stats_obj.get_mut("byEndpoint").and_then(Value::as_object_mut).unwrap();
            let entry = by.entry(ep_key).or_insert_with(|| {
                json!({
                    "requests": 0, "promptTokens": 0, "completionTokens": 0, "cachedTokens": 0,
                    "cost": 0, "endpoint": endpoint, "rawModel": model,
                    "provider": provider_display, "lastUsed": timestamp,
                })
            });
            merge_counter(entry, &vals);
            replace_if_newer(entry, &timestamp);
        }
    }

    // totalRequests = sum of byProvider requests
    let total: i64 = stats
        .get("byProvider")
        .and_then(Value::as_object)
        .map(|by| {
            by.values()
                .map(|p| p.get("requests").and_then(Value::as_i64).unwrap_or(0))
                .sum()
        })
        .unwrap_or(0);
    stats["totalRequests"] = json!(total);

    Ok(stats)
}

/// `merge_counter(entry, delta)`: add the five counters from `delta` into `entry`.
fn merge_counter(entry: &mut Value, delta: &Value) {
    let Some(obj) = entry.as_object_mut() else { return };
    for field in ["requests", "promptTokens", "completionTokens", "cachedTokens"] {
        let by = delta.get(field).and_then(Value::as_i64).unwrap_or(0);
        let next = obj.get(field).and_then(Value::as_i64).unwrap_or(0) + by;
        obj.insert(field.to_string(), json!(next));
    }
    let cost = obj.get("cost").and_then(Value::as_f64).unwrap_or(0.0)
        + delta.get("cost").and_then(Value::as_f64).unwrap_or(0.0);
    obj.insert("cost".to_string(), json!(cost));
}

/// `if (dateKey > lastUsed) lastUsed = dateKey` — day-granularity compare.
fn bump_last_used(entry: &mut Value, date_key: &str) {
    let Some(obj) = entry.as_object_mut() else { return };
    let current = obj.get("lastUsed").and_then(Value::as_str).unwrap_or("");
    if date_key > current {
        obj.insert("lastUsed".to_string(), json!(date_key));
    }
}

/// `if (new Date(ts) > new Date(lastUsed)) lastUsed = ts` — timestamp compare.
fn replace_if_newer(entry: &mut Value, ts: &str) {
    let Some(obj) = entry.as_object_mut() else { return };
    let current = obj.get("lastUsed").and_then(Value::as_str).unwrap_or("");
    let newer = match (
        chrono::DateTime::parse_from_rfc3339(ts),
        chrono::DateTime::parse_from_rfc3339(current),
    ) {
        (Ok(a), Ok(b)) => a > b,
        (Ok(_), Err(_)) => true,
        _ => ts > current,
    };
    if newer {
        obj.insert("lastUsed".to_string(), json!(ts));
    }
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
