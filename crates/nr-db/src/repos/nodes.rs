use crate::connection::Db;
use crate::helpers::{parse_json, stringify_json};
use nr_core::Result;
use serde_json::{json, Value};
use uuid::Uuid;

/// Get all provider nodes with optional filtering
pub fn get_provider_nodes(db: &Db, filter: Option<&Value>) -> Result<Vec<Value>> {
    let mut where_clauses = vec![];
    let mut params: Vec<String> = vec![];

    if let Some(f) = filter {
        if let Some(node_type) = f.get("type").and_then(|v| v.as_str()) {
            where_clauses.push("type = ?");
            params.push(node_type.to_string());
        }
    }

    let where_sql = if where_clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_clauses.join(" AND "))
    };

    let sql = format!(
        "SELECT id, type, name, data, createdAt, updatedAt FROM providerNodes{}",
        where_sql
    );

    let params_ref: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p as &dyn rusqlite::ToSql).collect();

    db.query_all(&sql, &params_ref, |row| {
        let data_str: String = row.get(3)?;
        let extra = parse_json(Some(data_str), json!({}));

        let mut result = extra.clone();
        result["id"] = Value::String(row.get(0)?);
        if let Ok(node_type) = row.get::<_, Option<String>>(1) {
            if let Some(t) = node_type {
                result["type"] = Value::String(t);
            }
        }
        if let Ok(name) = row.get::<_, Option<String>>(2) {
            if let Some(n) = name {
                result["name"] = Value::String(n);
            }
        }
        result["createdAt"] = Value::String(row.get(4)?);
        result["updatedAt"] = Value::String(row.get(5)?);

        Ok(result)
    })
}

/// Get a specific provider node by ID
pub fn get_provider_node_by_id(db: &Db, id: &str) -> Result<Option<Value>> {
    db.query_row(
        "SELECT id, type, name, data, createdAt, updatedAt FROM providerNodes WHERE id = ?",
        &[&id],
        |row| {
            let data_str: String = row.get(3)?;
            let extra = parse_json(Some(data_str), json!({}));

            let mut result = extra.clone();
            result["id"] = Value::String(row.get(0)?);
            if let Ok(node_type) = row.get::<_, Option<String>>(1) {
                if let Some(t) = node_type {
                    result["type"] = Value::String(t);
                }
            }
            if let Ok(name) = row.get::<_, Option<String>>(2) {
                if let Some(n) = name {
                    result["name"] = Value::String(n);
                }
            }
            result["createdAt"] = Value::String(row.get(4)?);
            result["updatedAt"] = Value::String(row.get(5)?);

            Ok(result)
        },
    )
}

/// Create a new provider node
pub fn create_provider_node(db: &Db, data: &Value) -> Result<Value> {
    let id = data
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let node_type = data.get("type").and_then(|v| v.as_str());
    let name = data.get("name").and_then(|v| v.as_str());
    let now = chrono::Utc::now().to_rfc3339();

    // Extract provider-specific data
    let mut extra_data = data.clone();
    let known_fields = vec!["id", "type", "name", "createdAt", "updatedAt"];
    for field in &known_fields {
        extra_data.as_object_mut().map(|o| o.remove(*field));
    }

    let data_json = stringify_json(&extra_data);

    db.execute_with_params(
        "INSERT INTO providerNodes(id, type, name, data, createdAt, updatedAt) VALUES(?, ?, ?, ?, ?, ?)",
        [
            id.clone(),
            node_type.unwrap_or("").to_string(),
            name.unwrap_or("").to_string(),
            data_json,
            now.clone(),
            now.clone(),
        ],
    )?;

    let mut result = extra_data;
    result["id"] = Value::String(id);
    if let Some(t) = node_type {
        result["type"] = Value::String(t.to_string());
    }
    if let Some(n) = name {
        result["name"] = Value::String(n.to_string());
    }
    result["createdAt"] = Value::String(now.clone());
    result["updatedAt"] = Value::String(now);

    Ok(result)
}

/// Update a provider node
pub fn update_provider_node(db: &Db, id: &str, data: &Value) -> Result<Option<Value>> {
    let existing = get_provider_node_by_id(db, id)?;
    match existing {
        None => Ok(None),
        Some(mut node) => {
            if let Some(node_type) = data.get("type").and_then(|v| v.as_str()) {
                node["type"] = Value::String(node_type.to_string());
            }
            if let Some(name) = data.get("name").and_then(|v| v.as_str()) {
                node["name"] = Value::String(name.to_string());
            }

            // Merge extra fields
            let mut extra_fields = data.clone();
            let known_fields = vec!["id", "type", "name", "createdAt", "updatedAt"];
            for field in &known_fields {
                extra_fields.as_object_mut().map(|o| o.remove(*field));
            }

            for (key, value) in extra_fields.as_object().unwrap_or(&Default::default()) {
                node[key] = value.clone();
            }

            node["updatedAt"] = Value::String(chrono::Utc::now().to_rfc3339());

            // Prepare data for storage
            let mut extra_data = node.clone();
            for field in &known_fields[..3] {
                extra_data.as_object_mut().map(|o| o.remove(*field));
            }

            let data_json = stringify_json(&extra_data);

            db.execute_with_params(
                "UPDATE providerNodes SET type = ?, name = ?, data = ?, updatedAt = ? WHERE id = ?",
                [
                    node.get("type").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    node.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    data_json,
                    node.get("updatedAt").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    id.to_string(),
                ],
            )?;

            Ok(Some(node))
        }
    }
}

/// Delete a provider node.
///
/// Mirrors nodesRepo.deleteProviderNode: removes the node and its
/// `customModels` kv rows (`key LIKE '<id>|%'`) in one transaction.
pub fn delete_provider_node(db: &Db, id: &str) -> Result<bool> {
    db.with_tx(|tx| {
        let exists = tx.query_row(
            "SELECT 1 FROM providerNodes WHERE id = ?",
            &[&id],
            |_row| Ok(()),
        )?;
        if exists.is_none() {
            return Ok(false);
        }

        tx.execute_with_params("DELETE FROM providerNodes WHERE id = ?", [id])?;
        tx.execute_with_params(
            "DELETE FROM kv WHERE scope = 'customModels' AND key LIKE ?",
            [format!("{id}|%").as_str()],
        )?;
        Ok(true)
    })
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
        let created = create_provider_node(
            &db,
            &json!({
                "id": "node-1",
                "type": "openai-compatible",
                "name": "Node One",
                "baseUrl": "https://api.example.com",
                "apiType": "chat",
                "prefix": "acme"
            }),
        )
        .unwrap();
        assert_eq!(created["id"], json!("node-1"));
        assert_eq!(created["name"], json!("Node One"));

        // Extra (non-column) fields round-trip through the `data` blob.
        let fetched = get_provider_node_by_id(&db, "node-1").unwrap().unwrap();
        assert_eq!(fetched["baseUrl"], json!("https://api.example.com"));
        assert_eq!(fetched["apiType"], json!("chat"));
        assert_eq!(fetched["prefix"], json!("acme"));

        // Filter by type.
        let typed = get_provider_nodes(
            &db,
            Some(&json!({"type": "openai-compatible"})),
        )
        .unwrap();
        assert_eq!(typed.len(), 1);
        let none = get_provider_nodes(&db, Some(&json!({"type": "vertex"}))).unwrap();
        assert!(none.is_empty());

        // Update merges and keeps unspecified fields.
        let updated = update_provider_node(&db, "node-1", &json!({"name": "Renamed"}))
            .unwrap()
            .unwrap();
        assert_eq!(updated["name"], json!("Renamed"));
        assert_eq!(updated["baseUrl"], json!("https://api.example.com"));
        let refetched = get_provider_node_by_id(&db, "node-1").unwrap().unwrap();
        assert_eq!(refetched["name"], json!("Renamed"));
        assert_ne!(refetched["updatedAt"], fetched["updatedAt"]);

        // Update of a missing node returns None.
        assert!(update_provider_node(&db, "nope", &json!({"name": "x"}))
            .unwrap()
            .is_none());

        // Delete returns true once, then false.
        assert!(delete_provider_node(&db, "node-1").unwrap());
        assert!(!delete_provider_node(&db, "node-1").unwrap());
        assert!(get_provider_node_by_id(&db, "node-1").unwrap().is_none());
    }

    #[test]
    fn delete_removes_attached_custom_models() {
        let (_dir, db) = db();
        create_provider_node(
            &db,
            &json!({"id": "node-x", "type": "openai-compatible", "name": "X"}),
        )
        .unwrap();
        crate::repos::aliases::add_custom_model(&db, "node-x", "model-a", None, None, None, None)
            .unwrap();
        crate::repos::aliases::add_custom_model(&db, "other", "model-b", None, None, None, None)
            .unwrap();

        assert!(delete_provider_node(&db, "node-x").unwrap());

        let remaining = crate::repos::aliases::get_custom_models(&db).unwrap();
        assert_eq!(remaining.len(), 1, "only the other provider's model survives");
        assert_eq!(remaining[0]["id"], json!("model-b"));
    }
}
