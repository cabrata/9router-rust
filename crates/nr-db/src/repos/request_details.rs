use crate::connection::Db;
use crate::helpers::{parse_json, stringify_json};
use nr_core::Result;
use serde_json::{json, Value};

/// Save or update a request detail.
///
/// Mirrors requestDetailsRepo's INSERT ... ON CONFLICT(id) DO UPDATE: every
/// column is refreshed from the incoming record, not just `data`.
pub fn save_request_detail(db: &Db, detail: &Value) -> Result<()> {
    let id = detail
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| nr_core::Error::other("id is required"))?
        .to_string();

    let timestamp = detail
        .get("timestamp")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let provider = detail.get("provider").and_then(|v| v.as_str()).map(|s| s.to_string());
    let model = detail.get("model").and_then(|v| v.as_str()).map(|s| s.to_string());
    let connection_id = detail.get("connectionId").and_then(|v| v.as_str()).map(|s| s.to_string());
    let status = detail.get("status").and_then(|v| v.as_str()).map(|s| s.to_string());

    let data_json = stringify_json(detail);

    db.execute_with_params(
        "INSERT INTO requestDetails(id, timestamp, provider, model, connectionId, status, data) VALUES(?, ?, ?, ?, ?, ?, ?) ON CONFLICT(id) DO UPDATE SET timestamp = excluded.timestamp, provider = excluded.provider, model = excluded.model, connectionId = excluded.connectionId, status = excluded.status, data = excluded.data",
        rusqlite::params![
            id,
            timestamp,
            provider,
            model,
            connection_id,
            status,
            data_json,
        ],
    )?;

    Ok(())
}

/// Get request details with optional filtering and pagination.
/// Returns `{ details, pagination }` like getRequestDetails().
pub fn get_request_details(db: &Db, filter: Option<&Value>) -> Result<Vec<Value>> {
    Ok(get_request_details_page(db, filter)?
        .get("details")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

/// Paginated `getRequestDetails()`: filters, count, page window and the
/// `pagination` envelope (JS shape).
pub fn get_request_details_page(db: &Db, filter: Option<&Value>) -> Result<Value> {
    let mut conds: Vec<&str> = vec![];
    let mut params: Vec<String> = vec![];

    if let Some(f) = filter {
        let s = |key: &str| f.get(key).and_then(Value::as_str).filter(|v| !v.is_empty());
        if let Some(provider) = s("provider") {
            conds.push("provider = ?");
            params.push(provider.to_string());
        }
        if let Some(model) = s("model") {
            conds.push("model = ?");
            params.push(model.to_string());
        }
        if let Some(connection_id) = s("connectionId") {
            conds.push("connectionId = ?");
            params.push(connection_id.to_string());
        }
        if let Some(status) = s("status") {
            conds.push("status = ?");
            params.push(status.to_string());
        }
        // JS normalizes date filters with new Date(x).toISOString().
        if let Some(start) = s("startDate").and_then(normalize_iso_date) {
            conds.push("timestamp >= ?");
            params.push(start);
        }
        if let Some(end) = s("endDate").and_then(normalize_iso_date) {
            conds.push("timestamp <= ?");
            params.push(end);
        }
    }

    let where_sql = if conds.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conds.join(" AND "))
    };

    let total_items: i64 = db
        .query_row(
            &format!("SELECT COUNT(*) as c FROM requestDetails{where_sql}"),
            &params
                .iter()
                .map(|p| p as &dyn rusqlite::ToSql)
                .collect::<Vec<_>>(),
            |row| row.get(0),
        )?
        .unwrap_or(0);

    let page = filter
        .and_then(|f| f.get("page").and_then(Value::as_i64))
        .unwrap_or(1)
        .max(1);
    let page_size = filter
        .and_then(|f| f.get("pageSize").and_then(Value::as_i64))
        .unwrap_or(50)
        .max(1);
    let total_pages = (total_items + page_size - 1) / page_size;
    let offset = (page - 1) * page_size;

    let mut all_params: Vec<&dyn rusqlite::ToSql> = params
        .iter()
        .map(|p| p as &dyn rusqlite::ToSql)
        .collect();
    all_params.push(&page_size);
    all_params.push(&offset);

    let details = db.query_all(
        &format!(
            "SELECT data FROM requestDetails{where_sql} ORDER BY timestamp DESC LIMIT ? OFFSET ?"
        ),
        &all_params,
        |row| {
            let data_str: String = row.get(0)?;
            Ok(parse_json(Some(data_str), json!({})))
        },
    )?;

    Ok(json!({
        "details": details,
        "pagination": {
            "page": page,
            "pageSize": page_size,
            "totalItems": total_items,
            "totalPages": total_pages,
            "hasNext": page < total_pages,
            "hasPrev": page > 1,
        }
    }))
}

/// `new Date(value).toISOString()`, returning None when unparseable
/// (JS would throw; callers here treat it as "no filter" instead of 500ing).
fn normalize_iso_date(value: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&chrono::Utc).to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .ok()
        .or_else(|| {
            // Also accept a bare date or naive datetime, like `new Date("2026-01-01")`.
            chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .map(|dt| {
                    chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(dt, chrono::Utc)
                        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                })
        })
}

/// Get a specific request detail by ID
pub fn get_request_detail_by_id(db: &Db, id: &str) -> Result<Option<Value>> {
    db.query_row(
        "SELECT data FROM requestDetails WHERE id = ?",
        &[&id],
        |row| {
            let data_str: String = row.get(0)?;
            let data = parse_json(Some(data_str), json!({}));
            Ok(data)
        },
    )
}

/// Get distinct providers from request details
pub fn get_distinct_providers(db: &Db) -> Result<Vec<String>> {
    db.query_all(
        "SELECT DISTINCT provider FROM requestDetails WHERE provider IS NOT NULL ORDER BY provider",
        &[],
        |row| {
            let provider: String = row.get(0)?;
            Ok(provider)
        },
    )
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

    fn detail(id: &str, ts: &str, provider: &str, model: &str) -> Value {
        json!({
            "id": id,
            "timestamp": ts,
            "provider": provider,
            "model": model,
            "connectionId": "conn-1",
            "status": "success",
            "latency": {"totalMs": 1200},
            "tokens": {"prompt_tokens": 10, "completion_tokens": 20},
            "request": {"messages": [{"role": "user", "content": "hi"}]},
        })
    }

    #[test]
    fn save_and_read_roundtrip() {
        let (_dir, db) = db();
        let record = detail("d1", "2026-01-01T00:00:00.000Z", "openai", "gpt-5");
        save_request_detail(&db, &record).unwrap();

        let fetched = get_request_detail_by_id(&db, "d1").unwrap().expect("row");
        assert_eq!(fetched["provider"], json!("openai"));
        assert_eq!(fetched["model"], json!("gpt-5"));
        assert_eq!(fetched["latency"]["totalMs"], json!(1200));
        assert_eq!(fetched["tokens"]["completion_tokens"], json!(20));

        let all = get_request_details(&db, None).unwrap();
        assert_eq!(all.len(), 1);

        // Missing id is an error, like the JS write path.
        assert!(save_request_detail(&db, &json!({"model": "x"})).is_err());
        assert!(get_request_detail_by_id(&db, "missing").unwrap().is_none());
    }

    #[test]
    fn upsert_refreshes_every_column() {
        let (_dir, db) = db();
        let mut record = detail("d1", "2026-01-01T00:00:00.000Z", "openai", "gpt-5");
        save_request_detail(&db, &record).unwrap();

        // Same id, different provider/model/timestamp/status: the JS upsert
        // updates all of them, so filters must see the new values.
        record["provider"] = json!("anthropic");
        record["model"] = json!("claude-opus-4.7");
        record["timestamp"] = json!("2026-01-02T00:00:00.000Z");
        record["status"] = json!("error");
        save_request_detail(&db, &record).unwrap();

        let by_old_provider = get_request_details(&db, Some(&json!({"provider": "openai"}))).unwrap();
        assert!(by_old_provider.is_empty(), "provider column was refreshed");
        let by_new = get_request_details(&db, Some(&json!({"provider": "anthropic"}))).unwrap();
        assert_eq!(by_new.len(), 1);
        assert_eq!(by_new[0]["model"], json!("claude-opus-4.7"));
        assert_eq!(by_new[0]["status"], json!("error"));
    }

    #[test]
    fn filters_cover_every_supported_key() {
        let (_dir, db) = db();
        let mut second = detail("d2", "2026-01-03T00:00:00.000Z", "openai", "gpt-5-mini");
        second["connectionId"] = json!("conn-2");
        second["status"] = json!("error");
        save_request_detail(&db, &detail("d1", "2026-01-01T00:00:00.000Z", "openai", "gpt-5")).unwrap();
        save_request_detail(&db, &second).unwrap();

        for (filter, expected) in [
            (json!({"model": "gpt-5-mini"}), 1usize),
            (json!({"connectionId": "conn-2"}), 1),
            (json!({"status": "error"}), 1),
            (json!({"provider": "openai"}), 2),
            (json!({"startDate": "2026-01-02T00:00:00.000Z"}), 1),
            (json!({"endDate": "2026-01-02T00:00:00.000Z"}), 1),
        ] {
            let rows = get_request_details(&db, Some(&filter)).unwrap();
            assert_eq!(rows.len(), expected, "filter {filter}");
        }
    }

    #[test]
    fn pagination_envelope_and_ordering() {
        let (_dir, db) = db();
        for i in 0..5 {
            save_request_detail(
                &db,
                &detail(
                    &format!("d{i}"),
                    &format!("2026-01-0{}T00:00:00.000Z", i + 1),
                    "openai",
                    "gpt-5",
                ),
            )
            .unwrap();
        }

        let page1 = get_request_details_page(&db, Some(&json!({"page": 1, "pageSize": 2}))).unwrap();
        assert_eq!(page1["pagination"]["page"], json!(1));
        assert_eq!(page1["pagination"]["pageSize"], json!(2));
        assert_eq!(page1["pagination"]["totalItems"], json!(5));
        assert_eq!(page1["pagination"]["totalPages"], json!(3));
        assert_eq!(page1["pagination"]["hasNext"], json!(true));
        assert_eq!(page1["pagination"]["hasPrev"], json!(false));
        // Newest first.
        assert_eq!(page1["details"][0]["id"], json!("d4"));
        assert_eq!(page1["details"][1]["id"], json!("d3"));

        let last = get_request_details_page(&db, Some(&json!({"page": 3, "pageSize": 2}))).unwrap();
        assert_eq!(last["details"].as_array().unwrap().len(), 1);
        assert_eq!(last["pagination"]["hasNext"], json!(false));
        assert_eq!(last["pagination"]["hasPrev"], json!(true));
    }

    #[test]
    fn distinct_providers_sorted_and_null_skipped() {
        let (_dir, db) = db();
        save_request_detail(&db, &detail("d1", "2026-01-01T00:00:00.000Z", "openai", "m")).unwrap();
        save_request_detail(&db, &detail("d2", "2026-01-02T00:00:00.000Z", "anthropic", "m")).unwrap();
        // A row with no provider must not appear.
        let mut no_provider = detail("d3", "2026-01-03T00:00:00.000Z", "x", "m");
        no_provider.as_object_mut().unwrap().remove("provider");
        save_request_detail(&db, &no_provider).unwrap();

        let providers = get_distinct_providers(&db).unwrap();
        assert_eq!(providers, vec!["anthropic".to_string(), "openai".to_string()]);
    }

    #[test]
    fn date_normalization_accepts_bare_dates() {
        assert_eq!(
            normalize_iso_date("2026-01-01"),
            Some("2026-01-01T00:00:00.000Z".to_string())
        );
        assert_eq!(
            normalize_iso_date("2026-01-01T05:06:07.000Z"),
            Some("2026-01-01T05:06:07.000Z".to_string())
        );
        assert_eq!(normalize_iso_date("not-a-date"), None);
    }
}
