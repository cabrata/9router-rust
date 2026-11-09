use crate::connection::Db;
use crate::helpers::{parse_json, stringify_json};
use nr_core::Result;
use serde_json::{json, Value};
use uuid::Uuid;

/// Get proxy pools with optional filtering
pub fn get_proxy_pools(db: &Db, filter: Option<&Value>) -> Result<Vec<Value>> {
    let mut where_clauses = vec![];
    let mut params: Vec<String> = vec![];

    if let Some(f) = filter {
        if let Some(is_active) = f.get("isActive").and_then(|v| v.as_bool()) {
            where_clauses.push("isActive = ?");
            params.push(if is_active { "1" } else { "0" }.to_string());
        }
        if let Some(test_status) = f.get("testStatus").and_then(|v| v.as_str()) {
            where_clauses.push("testStatus = ?");
            params.push(test_status.to_string());
        }
    }

    let where_sql = if where_clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_clauses.join(" AND "))
    };

    let sql = format!(
        "SELECT id, isActive, testStatus, data, createdAt, updatedAt FROM proxyPools{}",
        where_sql
    );

    let params_ref: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
    let mut results = db.query_all(&sql, &params_ref, |row| {
        let data_str: String = row.get(3)?;
        let extra = parse_json(Some(data_str), json!({}));

        let mut result = extra.clone();
        result["id"] = Value::String(row.get(0)?);
        result["isActive"] = Value::Bool(row.get::<_, i32>(1)? == 1);
        if let Ok(test_status) = row.get::<_, Option<String>>(2) {
            if let Some(ts) = test_status {
                result["testStatus"] = Value::String(ts);
            }
        }
        result["createdAt"] = Value::String(row.get(4)?);
        result["updatedAt"] = Value::String(row.get(5)?);

        Ok(result)
    })?;

    // Sort by updatedAt descending
    results.sort_by(|a, b| {
        let a_time = a.get("updatedAt").and_then(|v| v.as_str()).unwrap_or("");
        let b_time = b.get("updatedAt").and_then(|v| v.as_str()).unwrap_or("");
        b_time.cmp(a_time)
    });

    Ok(results)
}

/// Get a specific proxy pool by ID
pub fn get_proxy_pool_by_id(db: &Db, id: &str) -> Result<Option<Value>> {
    db.query_row(
        "SELECT id, isActive, testStatus, data, createdAt, updatedAt FROM proxyPools WHERE id = ?",
        &[&id],
        |row| {
            let data_str: String = row.get(3)?;
            let extra = parse_json(Some(data_str), json!({}));

            let mut result = extra.clone();
            result["id"] = Value::String(row.get(0)?);
            result["isActive"] = Value::Bool(row.get::<_, i32>(1)? == 1);
            if let Ok(test_status) = row.get::<_, Option<String>>(2) {
                if let Some(ts) = test_status {
                    result["testStatus"] = Value::String(ts);
                }
            }
            result["createdAt"] = Value::String(row.get(4)?);
            result["updatedAt"] = Value::String(row.get(5)?);

            Ok(result)
        },
    )
}

/// Create a new proxy pool
pub fn create_proxy_pool(db: &Db, data: &Value) -> Result<Value> {
    let id = data
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let is_active = if let Some(v) = data.get("isActive") {
        v.as_bool().unwrap_or(true)
    } else {
        true
    };

    let test_status = data.get("testStatus").and_then(|v| v.as_str()).unwrap_or("unknown");
    let now = chrono::Utc::now().to_rfc3339();

    // Extract extra data
    let mut extra_data = data.clone();
    let known_fields = vec!["id", "isActive", "testStatus", "createdAt", "updatedAt"];
    for field in &known_fields {
        extra_data.as_object_mut().map(|o| o.remove(*field));
    }

    let data_json = stringify_json(&extra_data);

    db.execute_with_params(
        "INSERT INTO proxyPools(id, isActive, testStatus, data, createdAt, updatedAt) VALUES(?, ?, ?, ?, ?, ?)",
        [
            id.clone(),
            (if is_active { 1 } else { 0 }).to_string(),
            test_status.to_string(),
            data_json,
            now.clone(),
            now.clone(),
        ],
    )?;

    let mut result = extra_data;
    result["id"] = Value::String(id);
    result["isActive"] = Value::Bool(is_active);
    result["testStatus"] = Value::String(test_status.to_string());
    result["createdAt"] = Value::String(now.clone());
    result["updatedAt"] = Value::String(now);

    Ok(result)
}

/// Update a proxy pool
pub fn update_proxy_pool(db: &Db, id: &str, data: &Value) -> Result<Option<Value>> {
    let existing = get_proxy_pool_by_id(db, id)?;
    match existing {
        None => Ok(None),
        Some(mut pool) => {
            if let Some(is_active) = data.get("isActive").and_then(|v| v.as_bool()) {
                pool["isActive"] = Value::Bool(is_active);
            }
            if let Some(test_status) = data.get("testStatus").and_then(|v| v.as_str()) {
                pool["testStatus"] = Value::String(test_status.to_string());
            }

            // Merge extra fields
            let mut extra_fields = data.clone();
            let known_fields = vec!["id", "isActive", "testStatus", "createdAt", "updatedAt"];
            for field in &known_fields {
                extra_fields.as_object_mut().map(|o| o.remove(*field));
            }

            for (key, value) in extra_fields.as_object().unwrap_or(&Default::default()) {
                pool[key] = value.clone();
            }

            pool["updatedAt"] = Value::String(chrono::Utc::now().to_rfc3339());

            // Prepare data for storage
            let mut extra_data = pool.clone();
            for field in &known_fields[..3] {
                extra_data.as_object_mut().map(|o| o.remove(*field));
            }

            let data_json = stringify_json(&extra_data);

            db.execute_with_params(
                "UPDATE proxyPools SET isActive = ?, testStatus = ?, data = ?, updatedAt = ? WHERE id = ?",
                [
                    (if pool.get("isActive").and_then(|v| v.as_bool()).unwrap_or(true) { 1 } else { 0 }).to_string(),
                    pool.get("testStatus").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    data_json,
                    pool.get("updatedAt").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    id.to_string(),
                ],
            )?;

            Ok(Some(pool))
        }
    }
}

/// Delete a proxy pool
pub fn delete_proxy_pool(db: &Db, id: &str) -> Result<bool> {
    let rows = db.execute_with_params(
        "DELETE FROM proxyPools WHERE id = ?",
        [id],
    )?;
    Ok(rows > 0)
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

    #[test]
    fn create_get_update_delete_roundtrip() {
        let (_dir, db) = db();
        let created = create_proxy_pool(
            &db,
            &json!({
                "id": "pool-1",
                "name": "Primary",
                "proxyUrl": "http://proxy:8080",
                "noProxy": "localhost",
                "type": "http",
                "strictProxy": true,
                "testStatus": "unknown",
            }),
        )
        .unwrap();
        assert_eq!(created["id"], json!("pool-1"));
        assert_eq!(created["name"], json!("Primary"));
        // isActive defaults to true when omitted (JS: isActive !== undefined).
        assert_eq!(created["isActive"], json!(true));
        // Extra fields round-trip through the `data` blob.
        assert_eq!(created["proxyUrl"], json!("http://proxy:8080"));
        assert_eq!(created["strictProxy"], json!(true));

        let fetched = get_proxy_pool_by_id(&db, "pool-1").unwrap().unwrap();
        assert_eq!(fetched["isActive"], json!(true));
        assert_eq!(fetched["noProxy"], json!("localhost"));

        // Filter by isActive.
        let active = get_proxy_pools(&db, Some(&json!({"isActive": true}))).unwrap();
        assert_eq!(active.len(), 1);
        let inactive = get_proxy_pools(&db, Some(&json!({"isActive": false}))).unwrap();
        assert!(inactive.is_empty());

        // Update merges and keeps unspecified fields.
        let updated = update_proxy_pool(&db, "pool-1", &json!({"name": "Renamed", "isActive": false}))
            .unwrap()
            .unwrap();
        assert_eq!(updated["name"], json!("Renamed"));
        assert_eq!(updated["isActive"], json!(false));
        assert_eq!(updated["proxyUrl"], json!("http://proxy:8080"));
        let refetched = get_proxy_pool_by_id(&db, "pool-1").unwrap().unwrap();
        assert_eq!(refetched["isActive"], json!(false));

        // Update of a missing pool returns None.
        assert!(update_proxy_pool(&db, "nope", &json!({"name": "x"})).unwrap().is_none());

        // Delete returns true once, then false.
        assert!(delete_proxy_pool(&db, "pool-1").unwrap());
        assert!(!delete_proxy_pool(&db, "pool-1").unwrap());
        assert!(get_proxy_pool_by_id(&db, "pool-1").unwrap().is_none());
    }

    #[test]
    fn id_defaults_to_uuid() {
        let (_dir, db) = db();
        let created = create_proxy_pool(&db, &json!({"name": "Auto", "proxyUrl": "x"})).unwrap();
        let id = created["id"].as_str().unwrap();
        assert_eq!(id.len(), 36, "uuid v4 by default");
        assert!(get_proxy_pool_by_id(&db, id).unwrap().is_some());
    }
}
