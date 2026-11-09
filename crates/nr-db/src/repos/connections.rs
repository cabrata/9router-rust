use crate::connection::{Db, DbTx};
use crate::helpers::{parse_json, stringify_json};
use chrono::Utc;
use nr_core::Result;
use serde_json::{json, Value};
use uuid::Uuid;

const MODEL_LOCK_PREFIX: &str = "modelLock_";

/// Reset health state fields when transitioning to active status
fn reset_health_state_on_activation(existing: &Value, patch: &Value) -> Value {
    if patch.get("testStatus").and_then(|v| v.as_str()) != Some("active") {
        return patch.clone();
    }

    let mut normalized = patch.clone();
    if !normalized.as_object().unwrap_or(&Default::default()).contains_key("lastError") {
        normalized["lastError"] = Value::Null;
    }
    if !normalized.as_object().unwrap_or(&Default::default()).contains_key("lastErrorAt") {
        normalized["lastErrorAt"] = Value::Null;
    }
    normalized["errorCode"] = Value::Null;
    normalized["rateLimitedUntil"] = Value::Null;
    normalized["backoffLevel"] = Value::Number(0.into());

    // Clear model locks
    if let Some(existing_obj) = existing.as_object() {
        let keys_to_clear: Vec<String> = existing_obj.keys()
            .filter(|k| k.starts_with(MODEL_LOCK_PREFIX))
            .cloned()
            .collect();

        for key in keys_to_clear {
            normalized[key] = Value::Null;
        }
    }

    normalized
}

/// Upsert a connection (insert or replace)
fn upsert_connection(tx: &DbTx, conn: &Value) -> Result<()> {
    let id = conn.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let provider = conn.get("provider").and_then(|v| v.as_str()).unwrap_or("");
    let auth_type = conn.get("authType").and_then(|v| v.as_str()).unwrap_or("oauth");
    let name = conn.get("name").and_then(|v| v.as_str());
    let email = conn.get("email").and_then(|v| v.as_str());
    let priority = conn.get("priority").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
    let is_active = conn.get("isActive").and_then(|v| v.as_bool()).unwrap_or(true);
    let created_at = conn.get("createdAt").and_then(|v| v.as_str()).unwrap_or("");
    let updated_at = conn.get("updatedAt").and_then(|v| v.as_str()).unwrap_or("");

    // Extract provider-specific data
    let mut extra_data = conn.clone();
    let known_fields = vec!["id", "provider", "authType", "name", "email", "priority", "isActive", "createdAt", "updatedAt"];
    for field in &known_fields {
        extra_data.as_object_mut().map(|o| o.remove(*field));
    }

    let data_json = stringify_json(&extra_data);

    tx.execute_with_params(
        "INSERT INTO providerConnections(id, provider, authType, name, email, priority, isActive, data, createdAt, updatedAt) VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(id) DO UPDATE SET provider=excluded.provider, authType=excluded.authType, name=excluded.name, email=excluded.email, priority=excluded.priority, isActive=excluded.isActive, data=excluded.data, updatedAt=excluded.updatedAt",
        [
            id.to_string(),
            provider.to_string(),
            auth_type.to_string(),
            name.map(|s| s.to_string()).unwrap_or_default(),
            email.map(|s| s.to_string()).unwrap_or_default(),
            priority.to_string(),
            (if is_active { 1 } else { 0 }).to_string(),
            data_json,
            created_at.to_string(),
            updated_at.to_string(),
        ],
    )?;

    Ok(())
}

/// Reorder connections to have contiguous priorities (must be called in transaction)
fn reorder_connections_in_tx(tx: &DbTx, provider: &str) -> Result<()> {
    let mut conns = tx.query_all(
        "SELECT id, provider, authType, name, email, priority, isActive, data, createdAt, updatedAt FROM providerConnections WHERE provider = ? ORDER BY priority ASC, updatedAt DESC",
        &[&provider],
        |row| {
            let data_str: String = row.get(7)?;
            let extra = parse_json(Some(data_str), json!({}));
            let mut result = extra;
            result["id"] = Value::String(row.get(0)?);
            result["provider"] = Value::String(row.get(1)?);
            result["authType"] = Value::String(row.get(2)?);
            if let Ok(n) = row.get::<_, Option<String>>(3) {
                if let Some(val) = n {
                    result["name"] = Value::String(val);
                }
            }
            if let Ok(e) = row.get::<_, Option<String>>(4) {
                if let Some(val) = e {
                    result["email"] = Value::String(val);
                }
            }
            if let Ok(p) = row.get::<_, Option<i32>>(5) {
                if let Some(val) = p {
                    result["priority"] = Value::Number(val.into());
                }
            }
            result["isActive"] = Value::Bool(row.get::<_, i32>(6)? == 1);
            result["createdAt"] = Value::String(row.get(8)?);
            result["updatedAt"] = Value::String(row.get(9)?);
            Ok(result)
        },
    )?;

    for (i, mut conn) in conns.into_iter().enumerate() {
        let new_priority = (i + 1) as i32;
        let current_priority = conn.get("priority").and_then(|v| v.as_i64()).unwrap_or(0) as i32;

        if current_priority != new_priority {
            conn["priority"] = Value::Number(new_priority.into());
            let id = conn.get("id").and_then(|v| v.as_str()).unwrap_or("");
            tx.execute_with_params(
                "UPDATE providerConnections SET priority = ? WHERE id = ?",
                [new_priority.to_string(), id.to_string()],
            )?;
        }
    }

    Ok(())
}

/// Get provider connections with optional filtering
pub fn get_provider_connections(db: &Db, filter: Option<&Value>) -> Result<Vec<Value>> {
    let mut where_clauses = vec![];
    let mut params: Vec<String> = vec![];

    if let Some(f) = filter {
        if let Some(provider) = f.get("provider").and_then(|v| v.as_str()) {
            where_clauses.push("provider = ?");
            params.push(provider.to_string());
        }
        if let Some(is_active) = f.get("isActive").and_then(|v| v.as_bool()) {
            where_clauses.push("isActive = ?");
            params.push(if is_active { "1" } else { "0" }.to_string());
        }
    }

    let where_sql = if where_clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_clauses.join(" AND "))
    };

    let sql = format!(
        "SELECT id, provider, authType, name, email, priority, isActive, data, createdAt, updatedAt FROM providerConnections{}",
        where_sql
    );

    let params_ref: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
    let mut results = db.query_all(&sql, &params_ref, |row| {
        let data_str: String = row.get(7)?;
        let extra = parse_json(Some(data_str), json!({}));

        let mut result = extra.clone();
        result["id"] = Value::String(row.get(0)?);
        result["provider"] = Value::String(row.get(1)?);
        result["authType"] = Value::String(row.get(2)?);
        if let Ok(name) = row.get::<_, Option<String>>(3) {
            if let Some(n) = name {
                result["name"] = Value::String(n);
            }
        }
        if let Ok(email) = row.get::<_, Option<String>>(4) {
            if let Some(e) = email {
                result["email"] = Value::String(e);
            }
        }
        if let Ok(priority) = row.get::<_, Option<i32>>(5) {
            if let Some(p) = priority {
                result["priority"] = Value::Number(p.into());
            }
        }
        result["isActive"] = Value::Bool(row.get::<_, i32>(6)? == 1);
        result["createdAt"] = Value::String(row.get(8)?);
        result["updatedAt"] = Value::String(row.get(9)?);

        Ok(result)
    })?;

    // Sort by priority
    results.sort_by(|a, b| {
        let a_priority = a.get("priority").and_then(|v| v.as_i64()).unwrap_or(999);
        let b_priority = b.get("priority").and_then(|v| v.as_i64()).unwrap_or(999);
        a_priority.cmp(&b_priority)
    });

    Ok(results)
}

/// Get a specific provider connection by ID
pub fn get_provider_connection_by_id(db: &Db, id: &str) -> Result<Option<Value>> {
    db.query_row(
        "SELECT id, provider, authType, name, email, priority, isActive, data, createdAt, updatedAt FROM providerConnections WHERE id = ?",
        &[&id],
        |row| {
            let data_str: String = row.get(7)?;
            let extra = parse_json(Some(data_str), json!({}));

            let mut result = extra.clone();
            result["id"] = Value::String(row.get(0)?);
            result["provider"] = Value::String(row.get(1)?);
            result["authType"] = Value::String(row.get(2)?);
            if let Ok(name) = row.get::<_, Option<String>>(3) {
                if let Some(n) = name {
                    result["name"] = Value::String(n);
                }
            }
            if let Ok(email) = row.get::<_, Option<String>>(4) {
                if let Some(e) = email {
                    result["email"] = Value::String(e);
                }
            }
            if let Ok(priority) = row.get::<_, Option<i32>>(5) {
                if let Some(p) = priority {
                    result["priority"] = Value::Number(p.into());
                }
            }
            result["isActive"] = Value::Bool(row.get::<_, i32>(6)? == 1);
            result["createdAt"] = Value::String(row.get(8)?);
            result["updatedAt"] = Value::String(row.get(9)?);

            Ok(result)
        },
    )
}

/// Create a new provider connection with deduplication and atomic priority management
pub fn create_provider_connection(db: &Db, data: &Value) -> Result<Value> {
    let provider = data
        .get("provider")
        .and_then(|v| v.as_str())
        .ok_or_else(|| nr_core::Error::other("provider is required"))?;

    let auth_type = data
        .get("authType")
        .and_then(|v| v.as_str())
        .unwrap_or("oauth");

    let allow_overwrite = data
        .get("allowOverwrite")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let mut result = Value::Null;

    db.with_tx(|tx| {
        let now = Utc::now().to_rfc3339();

        // Check for existing connection by dedup rules
        let mut existing_option: Option<Value> = None;

        if auth_type == "oauth" {
            if let Some(email) = data.get("email").and_then(|v| v.as_str()) {
                // Query oauth connections with same email
                let conns = tx.query_all(
                    "SELECT id, provider, authType, name, email, priority, isActive, data, createdAt, updatedAt FROM providerConnections WHERE provider = ? AND authType = 'oauth' AND email = ?",
                    &[&provider, &email],
                    |row| {
                        let data_str: String = row.get(7)?;
                        let extra = parse_json(Some(data_str), json!({}));
                        let mut result = extra;
                        result["id"] = Value::String(row.get(0)?);
                        result["provider"] = Value::String(row.get(1)?);
                        result["authType"] = Value::String(row.get(2)?);
                        if let Ok(n) = row.get::<_, Option<String>>(3) {
                            if let Some(val) = n {
                                result["name"] = Value::String(val);
                            }
                        }
                        if let Ok(e) = row.get::<_, Option<String>>(4) {
                            if let Some(val) = e {
                                result["email"] = Value::String(val);
                            }
                        }
                        if let Ok(p) = row.get::<_, Option<i32>>(5) {
                            if let Some(val) = p {
                                result["priority"] = Value::Number(val.into());
                            }
                        }
                        result["isActive"] = Value::Bool(row.get::<_, i32>(6)? == 1);
                        result["createdAt"] = Value::String(row.get(8)?);
                        result["updatedAt"] = Value::String(row.get(9)?);
                        Ok(result)
                    },
                )?;

                // Find matching connection based on workspace ID or username
                if let Some(incoming_ws) = data.get("providerSpecificData").and_then(|psd| psd.get("chatgptAccountId")).and_then(|v| v.as_str()) {
                    for conn in &conns {
                        if let Some(existing_ws) = conn.get("providerSpecificData").and_then(|psd| psd.get("chatgptAccountId")).and_then(|v| v.as_str()) {
                            if incoming_ws == existing_ws {
                                existing_option = Some(conn.clone());
                                break;
                            }
                        }
                    }
                }

                // If no ws match, try username match
                if existing_option.is_none() {
                    if let Some(incoming_username) = data.get("providerSpecificData").and_then(|psd| psd.get("username")).and_then(|v| v.as_str()) {
                        for conn in &conns {
                            if let Some(existing_username) = conn.get("providerSpecificData").and_then(|psd| psd.get("username")).and_then(|v| v.as_str()) {
                                if incoming_username == existing_username {
                                    existing_option = Some(conn.clone());
                                    break;
                                }
                            }
                        }
                    }
                }

                // If still no match and no workspace/username on incoming, take bare email match
                if existing_option.is_none() {
                    for conn in &conns {
                        if !data.get("providerSpecificData").and_then(|psd| psd.get("chatgptAccountId")).is_some()
                            && !data.get("providerSpecificData").and_then(|psd| psd.get("username")).is_some()
                            && !conn.get("providerSpecificData").and_then(|psd| psd.get("chatgptAccountId")).is_some()
                            && !conn.get("providerSpecificData").and_then(|psd| psd.get("username")).is_some()
                        {
                            existing_option = Some(conn.clone());
                            break;
                        }
                    }
                }
            }
        } else if auth_type == "apikey" {
            if let Some(name) = data.get("name").and_then(|v| v.as_str()) {
                if let Some(existing) = tx.query_row(
                    "SELECT id, provider, authType, name, email, priority, isActive, data, createdAt, updatedAt FROM providerConnections WHERE provider = ? AND authType = 'apikey' AND name = ?",
                    &[&provider, &name],
                    |row| {
                        let data_str: String = row.get(7)?;
                        let extra = parse_json(Some(data_str), json!({}));
                        let mut result = extra;
                        result["id"] = Value::String(row.get(0)?);
                        result["provider"] = Value::String(row.get(1)?);
                        result["authType"] = Value::String(row.get(2)?);
                        if let Ok(n) = row.get::<_, Option<String>>(3) {
                            if let Some(val) = n {
                                result["name"] = Value::String(val);
                            }
                        }
                        if let Ok(e) = row.get::<_, Option<String>>(4) {
                            if let Some(val) = e {
                                result["email"] = Value::String(val);
                            }
                        }
                        if let Ok(p) = row.get::<_, Option<i32>>(5) {
                            if let Some(val) = p {
                                result["priority"] = Value::Number(val.into());
                            }
                        }
                        result["isActive"] = Value::Bool(row.get::<_, i32>(6)? == 1);
                        result["createdAt"] = Value::String(row.get(8)?);
                        result["updatedAt"] = Value::String(row.get(9)?);
                        Ok(result)
                    },
                )? {
                    existing_option = Some(existing);
                }
            }
        }

        // Handle existing connection
        if let Some(mut existing) = existing_option {
            if !allow_overwrite {
                return Err(nr_core::Error::other(
                    format!(
                        "A connection named \"{}\" already exists for provider \"{}\". Pass allowOverwrite: true to replace it.",
                        existing.get("name").and_then(|v| v.as_str()).unwrap_or("unnamed"),
                        provider
                    )
                ));
            }

            // Merge with reset health state on activation
            let normalized = reset_health_state_on_activation(&existing, data);
            let mut merged = existing.clone();
            for (k, v) in normalized.as_object().unwrap_or(&Default::default()) {
                merged[k] = v.clone();
            }
            merged["updatedAt"] = Value::String(now.clone());

            upsert_connection(tx, &merged)?;
            result = merged;
        } else {
            // Create new connection
            let id = Uuid::new_v4().to_string();

            // Get max priority for this provider
            let max_priority = tx.query_row(
                "SELECT MAX(priority) AS m FROM providerConnections WHERE provider = ?",
                &[&provider],
                |row| row.get::<_, Option<i32>>(0),
            )?.flatten().unwrap_or(0);

            let priority = data.get("priority").and_then(|v| v.as_i64()).unwrap_or((max_priority + 1) as i64) as i32;

            let is_active = data.get("isActive").and_then(|v| v.as_bool()).unwrap_or(true);

            let name = data.get("name").and_then(|v| v.as_str()).map(|s| s.to_string());
            let email = data.get("email").and_then(|v| v.as_str()).map(|s| s.to_string());

            // Extract provider-specific data
            let mut extra_data = data.clone();
            let known_fields = vec!["provider", "authType", "name", "email", "priority", "isActive", "allowOverwrite"];
            for field in &known_fields {
                extra_data.as_object_mut().map(|o| o.remove(*field));
            }

            let data_json = stringify_json(&extra_data);

            tx.execute_with_params(
                "INSERT INTO providerConnections(id, provider, authType, name, email, priority, isActive, data, createdAt, updatedAt) VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                [
                    id.clone(),
                    provider.to_string(),
                    auth_type.to_string(),
                    name.clone().unwrap_or_default(),
                    email.clone().unwrap_or_default(),
                    priority.to_string(),
                    (if is_active { 1 } else { 0 }).to_string(),
                    data_json,
                    now.clone(),
                    now.clone(),
                ],
            )?;

            let mut conn_result = extra_data;
            conn_result["id"] = Value::String(id);
            conn_result["provider"] = Value::String(provider.to_string());
            conn_result["authType"] = Value::String(auth_type.to_string());
            if let Some(n) = name {
                conn_result["name"] = Value::String(n);
            }
            if let Some(e) = email {
                conn_result["email"] = Value::String(e);
            }
            conn_result["priority"] = Value::Number(priority.into());
            conn_result["isActive"] = Value::Bool(is_active);
            conn_result["createdAt"] = Value::String(now.clone());
            conn_result["updatedAt"] = Value::String(now);

            result = conn_result;
        }

        Ok(())
    })?;

    Ok(result)
}

/// Update a provider connection with atomic token refresh merge
pub fn update_provider_connection(db: &Db, id: &str, data: &Value) -> Result<Option<Value>> {
    let mut result = None;

    db.with_tx(|tx| {
        if let Some(existing) = tx.query_row(
            "SELECT id, provider, authType, name, email, priority, isActive, data, createdAt, updatedAt FROM providerConnections WHERE id = ?",
            &[&id],
            |row| {
                let data_str: String = row.get(7)?;
                let extra = parse_json(Some(data_str), json!({}));
                let mut result = extra;
                result["id"] = Value::String(row.get(0)?);
                result["provider"] = Value::String(row.get(1)?);
                result["authType"] = Value::String(row.get(2)?);
                if let Ok(n) = row.get::<_, Option<String>>(3) {
                    if let Some(val) = n {
                        result["name"] = Value::String(val);
                    }
                }
                if let Ok(e) = row.get::<_, Option<String>>(4) {
                    if let Some(val) = e {
                        result["email"] = Value::String(val);
                    }
                }
                if let Ok(p) = row.get::<_, Option<i32>>(5) {
                    if let Some(val) = p {
                        result["priority"] = Value::Number(val.into());
                    }
                }
                result["isActive"] = Value::Bool(row.get::<_, i32>(6)? == 1);
                result["createdAt"] = Value::String(row.get(8)?);
                result["updatedAt"] = Value::String(row.get(9)?);
                Ok(result)
            },
        )? {
            let normalized = reset_health_state_on_activation(&existing, data);
            let mut merged = existing.clone();
            for (k, v) in normalized.as_object().unwrap_or(&Default::default()) {
                merged[k] = v.clone();
            }
            merged["updatedAt"] = Value::String(Utc::now().to_rfc3339());

            upsert_connection(tx, &merged)?;

            // If priority changed, reorder the provider's connections
            if data.get("priority").is_some() {
                if let Some(provider) = merged.get("provider").and_then(|v| v.as_str()) {
                    reorder_connections_in_tx(tx, provider)?;
                }
            }

            result = Some(merged);
        }

        Ok(())
    })?;

    Ok(result)
}

/// Delete a provider connection and reorder remaining priorities
pub fn delete_provider_connection(db: &Db, id: &str) -> Result<bool> {
    let mut deleted = false;

    db.with_tx(|tx| {
        // Get the provider ID before deletion
        let provider_option = tx.query_row(
            "SELECT provider FROM providerConnections WHERE id = ?",
            &[&id],
            |row| row.get::<_, String>(0),
        )?;

        if let Some(provider) = provider_option {
            tx.execute_with_params(
                "DELETE FROM providerConnections WHERE id = ?",
                [id],
            )?;
            reorder_connections_in_tx(tx, &provider)?;
            deleted = true;
        }

        Ok(())
    })?;

    Ok(deleted)
}

/// Delete all connections for a provider
pub fn delete_provider_connections_by_provider(db: &Db, provider_id: &str) -> Result<usize> {
    db.execute_with_params(
        "DELETE FROM providerConnections WHERE provider = ?",
        [provider_id],
    )
}

/// Reorder provider connections to have contiguous priorities
pub fn reorder_provider_connections(db: &Db, provider_id: &str) -> Result<()> {
    db.with_tx(|tx| {
        reorder_connections_in_tx(tx, provider_id)
    })
}

/// Clean up null/undefined fields in provider connections
pub fn cleanup_provider_connections(db: &Db) -> Result<usize> {
    let mut cleaned = 0;

    db.with_tx(|tx| {
        let conns = tx.query_all(
            "SELECT id, provider, authType, name, email, priority, isActive, data, createdAt, updatedAt FROM providerConnections",
            &[],
            |row| {
                let data_str: String = row.get(7)?;
                let extra = parse_json(Some(data_str), json!({}));
                let mut result = extra;
                result["id"] = Value::String(row.get(0)?);
                result["provider"] = Value::String(row.get(1)?);
                result["authType"] = Value::String(row.get(2)?);
                if let Ok(n) = row.get::<_, Option<String>>(3) {
                    if let Some(val) = n {
                        result["name"] = Value::String(val);
                    }
                }
                if let Ok(e) = row.get::<_, Option<String>>(4) {
                    if let Some(val) = e {
                        result["email"] = Value::String(val);
                    }
                }
                if let Ok(p) = row.get::<_, Option<i32>>(5) {
                    if let Some(val) = p {
                        result["priority"] = Value::Number(val.into());
                    }
                }
                result["isActive"] = Value::Bool(row.get::<_, i32>(6)? == 1);
                result["createdAt"] = Value::String(row.get(8)?);
                result["updatedAt"] = Value::String(row.get(9)?);
                Ok(result)
            },
        )?;

        let fields_to_check = vec![
            "displayName", "email", "globalPriority", "defaultModel",
            "accessToken", "refreshToken", "expiresAt", "tokenType",
            "scope", "projectId", "apiKey", "testStatus",
            "lastTested", "lastError", "lastErrorAt", "rateLimitedUntil", "expiresIn",
            "consecutiveUseCount",
        ];

        for mut conn in conns {
            let mut dirty = false;
            for field in &fields_to_check {
                if let Some(val) = conn.get(*field) {
                    if val.is_null() {
                        conn.as_object_mut().map(|o| o.remove(*field));
                        cleaned += 1;
                        dirty = true;
                    }
                }
            }

            if let Some(psd) = conn.get("providerSpecificData") {
                if psd.as_object().map_or(false, |o| o.is_empty()) {
                    conn.as_object_mut().map(|o| o.remove("providerSpecificData"));
                    cleaned += 1;
                    dirty = true;
                }
            }

            if dirty {
                upsert_connection(tx, &conn)?;
            }
        }

        Ok(())
    })?;

    Ok(cleaned)
}
