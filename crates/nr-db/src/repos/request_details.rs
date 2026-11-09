use crate::connection::Db;
use crate::helpers::{parse_json, stringify_json};
use nr_core::Result;
use serde_json::{json, Value};

/// Save or update a request detail
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
        "INSERT INTO requestDetails(id, timestamp, provider, model, connectionId, status, data) VALUES(?, ?, ?, ?, ?, ?, ?) ON CONFLICT(id) DO UPDATE SET data = excluded.data",
        [
            id,
            timestamp,
            provider.unwrap_or_default(),
            model.unwrap_or_default(),
            connection_id.unwrap_or_default(),
            status.unwrap_or_default(),
            data_json,
        ],
    )?;

    Ok(())
}

/// Get request details with optional filtering
pub fn get_request_details(db: &Db, filter: Option<&Value>) -> Result<Vec<Value>> {
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
        "SELECT data FROM requestDetails{} ORDER BY timestamp DESC LIMIT 200",
        where_sql
    );

    let params_ref: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p as &dyn rusqlite::ToSql).collect();

    db.query_all(&sql, &params_ref, |row| {
        let data_str: String = row.get(0)?;
        let data = parse_json(Some(data_str), json!({}));
        Ok(data)
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
