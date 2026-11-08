use crate::connection::Db;
use crate::helpers::{normalize_allow_list, parse_json};
use nr_core::Result;
use serde_json::{json, Value};
use uuid::Uuid;

const MAX_LIMIT: u32 = 1_000_000;

/// API Key with all metadata
#[derive(Clone, Debug)]
pub struct ApiKey {
    pub id: String,
    pub key: String,
    pub name: Option<String>,
    pub machine_id: Option<String>,
    pub is_active: bool,
    pub expired_at: Option<String>,
    pub limit_concurrent: u32,
    pub rpm: u32,
    pub created_at: String,
    pub allowed_models: Vec<String>,
    pub allowed_endpoints: Vec<String>,
    pub access: KeyAccess,
}

#[derive(Clone, Debug)]
pub struct KeyAccess {
    pub restricted: bool,
    pub allow: Vec<String>,
}

impl Default for KeyAccess {
    fn default() -> Self {
        KeyAccess {
            restricted: false,
            allow: vec![],
        }
    }
}

/// Validate and normalize API key limits
pub fn normalize_api_key_limits(data: &Value) -> Result<Value> {
    let mut result = json!({});

    if let Some(expired_at) = data.get("expiredAt") {
        if expired_at.is_null() {
            result["expiredAt"] = Value::Null;
        } else if let Some(s) = expired_at.as_str() {
            // Validate ISO date format
            if let Ok(_) = chrono::DateTime::parse_from_rfc3339(s) {
                result["expiredAt"] = Value::String(
                    chrono::DateTime::parse_from_rfc3339(s)
                        .unwrap()
                        .to_rfc3339(),
                );
            } else {
                return Err(
                    nr_core::Error::other("expiredAt must be null or a valid ISO date-time")
                );
            }
        }
    }

    for field in &["limitConcurrent", "rpm"] {
        if let Some(val) = data.get(field) {
            if let Some(num) = val.as_i64() {
                if num < 0 || num > MAX_LIMIT as i64 {
                    return Err(nr_core::Error::other(format!(
                        "{} must be an integer from 0 to {}",
                        field, MAX_LIMIT
                    )));
                }
                result[field] = Value::Number(num.into());
            }
        }
    }

    Ok(result)
}

/// Get all API keys ordered by creation time
pub fn get_api_keys(db: &Db) -> Result<Vec<ApiKey>> {
    db.query_all(
        "SELECT * FROM apiKeys ORDER BY createdAt ASC",
        &[],
        |row| {
            Ok(ApiKey {
                id: row.get(0)?,
                key: row.get(1)?,
                name: row.get(2)?,
                machine_id: row.get(3)?,
                is_active: row.get::<_, i32>(4)? == 1,
                expired_at: row.get(5)?,
                limit_concurrent: row.get::<_, i32>(6)? as u32,
                rpm: row.get::<_, i32>(7)? as u32,
                created_at: row.get(8)?,
                allowed_models: parse_json(row.get(9)?, json!([]))
                    .as_array()
                    .map(|v| {
                        v.iter()
                            .filter_map(|x| x.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default(),
                allowed_endpoints: parse_json(row.get(10)?, json!([]))
                    .as_array()
                    .map(|v| {
                        v.iter()
                            .filter_map(|x| x.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default(),
                access: key_access_from_columns(row.get(11)?, row.get(12)?),
            })
        },
    )
}

/// Get a specific API key by ID
pub fn get_api_key_by_id(db: &Db, id: &str) -> Result<Option<ApiKey>> {
    db.query_row(
        "SELECT * FROM apiKeys WHERE id = ?",
        &[&id],
        |row| {
            Ok(ApiKey {
                id: row.get(0)?,
                key: row.get(1)?,
                name: row.get(2)?,
                machine_id: row.get(3)?,
                is_active: row.get::<_, i32>(4)? == 1,
                expired_at: row.get(5)?,
                limit_concurrent: row.get::<_, i32>(6)? as u32,
                rpm: row.get::<_, i32>(7)? as u32,
                created_at: row.get(8)?,
                allowed_models: parse_json(row.get(9)?, json!([]))
                    .as_array()
                    .map(|v| {
                        v.iter()
                            .filter_map(|x| x.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default(),
                allowed_endpoints: parse_json(row.get(10)?, json!([]))
                    .as_array()
                    .map(|v| {
                        v.iter()
                            .filter_map(|x| x.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default(),
                access: key_access_from_columns(row.get(11)?, row.get(12)?),
            })
        },
    )
}

/// Get a specific API key by its key value
pub fn get_api_key_by_key(db: &Db, key: &str) -> Result<Option<ApiKey>> {
    if key.is_empty() {
        return Ok(None);
    }

    db.query_row(
        "SELECT * FROM apiKeys WHERE key = ?",
        &[&key],
        |row| {
            Ok(ApiKey {
                id: row.get(0)?,
                key: row.get(1)?,
                name: row.get(2)?,
                machine_id: row.get(3)?,
                is_active: row.get::<_, i32>(4)? == 1,
                expired_at: row.get(5)?,
                limit_concurrent: row.get::<_, i32>(6)? as u32,
                rpm: row.get::<_, i32>(7)? as u32,
                created_at: row.get(8)?,
                allowed_models: parse_json(row.get(9)?, json!([]))
                    .as_array()
                    .map(|v| {
                        v.iter()
                            .filter_map(|x| x.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default(),
                allowed_endpoints: parse_json(row.get(10)?, json!([]))
                    .as_array()
                    .map(|v| {
                        v.iter()
                            .filter_map(|x| x.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default(),
                access: key_access_from_columns(row.get(11)?, row.get(12)?),
            })
        },
    )
}

/// Validate an API key (check active and expiration)
pub fn validate_api_key(db: &Db, key: &str) -> Result<bool> {
    let result = db.query_row(
        "SELECT isActive, expiredAt FROM apiKeys WHERE key = ?",
        &[&key],
        |row| {
            let active = row.get::<_, i32>(0)? == 1;
            let expired_at: Option<String> = row.get(1)?;
            Ok((active, expired_at))
        },
    )?;

    match result {
        Some((active, expired_at)) => {
            if !active {
                return Ok(false);
            }
            if let Some(exp_str) = expired_at {
                if let Ok(exp_time) = chrono::DateTime::parse_from_rfc3339(&exp_str) {
                    return Ok(exp_time.timestamp_millis() > chrono::Utc::now().timestamp_millis());
                }
            }
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Create a new API key
pub fn create_api_key(
    db: &Db,
    name: Option<String>,
    machine_id: String,
    _settings: Option<Value>,
) -> Result<ApiKey> {
    if machine_id.is_empty() {
        return Err(nr_core::Error::other("machineId is required"));
    }

    let id = Uuid::new_v4().to_string();
    let key = format!("sk-{}", Uuid::new_v4().to_string().replace("-", "")[..32].to_string());
    let now = chrono::Utc::now().to_rfc3339();
    let name_for_db = name.clone().unwrap_or_default();

    let cols = key_access_to_columns(&KeyAccess::default());

    db.execute_with_params(
        "INSERT INTO apiKeys(id, key, name, machineId, isActive, createdAt, accessRestricted, accessAllow) VALUES(?, ?, ?, ?, 1, ?, ?, ?)",
        [
            id.clone(),
            key.clone(),
            name_for_db,
            machine_id.clone(),
            now.clone(),
            cols.0.to_string(),
            cols.1,
        ],
    )?;

    Ok(ApiKey {
        id,
        key,
        name,
        machine_id: Some(machine_id),
        is_active: true,
        expired_at: None,
        limit_concurrent: 0,
        rpm: 0,
        created_at: now,
        allowed_models: vec![],
        allowed_endpoints: vec![],
        access: KeyAccess::default(),
    })
}

/// Delete an API key by ID
pub fn delete_api_key(db: &Db, id: &str) -> Result<bool> {
    let rows = db.execute_with_params(
        "DELETE FROM apiKeys WHERE id = ?",
        [id],
    )?;
    Ok(rows > 0)
}

// ─── Helpers ────────────────────────────────────────────────────────────

fn key_access_from_columns(restricted: Option<i32>, allow: Option<String>) -> KeyAccess {
    let restricted_bool = restricted.map(|v| v == 1).unwrap_or(false);
    if !restricted_bool {
        return KeyAccess {
            restricted: false,
            allow: vec![],
        };
    }

    let allow_list = allow
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or_default();

    KeyAccess {
        restricted: true,
        allow: normalize_allow_list(Some(allow_list)),
    }
}

fn key_access_to_columns(access: &KeyAccess) -> (i32, String) {
    let restricted = if access.restricted { 1 } else { 0 };
    let allow = serde_json::to_string(&access.allow).unwrap_or_default();
    (restricted, allow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_create_api_key() {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        let key = create_api_key(&db, Some("test".to_string()), "machine-1".to_string(), None).unwrap();
        assert_eq!(key.name, Some("test".to_string()));
        assert!(key.is_active);
    }

    #[test]
    fn test_get_api_key_by_key() {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        let key = create_api_key(&db, Some("test".to_string()), "machine-1".to_string(), None).unwrap();
        let retrieved = get_api_key_by_key(&db, &key.key).unwrap();
        assert!(retrieved.is_some());
    }
}
