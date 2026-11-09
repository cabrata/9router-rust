use crate::connection::Db;
use crate::helpers::{parse_json, stringify_json};
use nr_core::Result;
use serde_json::{json, Value};
use std::collections::HashSet;

const SCOPE: &str = "disabledModels";

/// Get all disabled models per provider
pub fn get_disabled_models(db: &Db) -> Result<Value> {
    let rows = db.query_all(
        "SELECT key, value FROM kv WHERE scope = ?",
        &[&SCOPE],
        |row| {
            let key: String = row.get(0)?;
            let value: String = row.get(1)?;
            Ok((key, value))
        },
    )?;

    let mut out = serde_json::Map::new();
    for (key, value) in rows {
        let parsed = parse_json(Some(value), json!([]));
        out.insert(key, parsed);
    }

    Ok(Value::Object(out))
}

/// Get disabled models for a specific provider
pub fn get_disabled_by_provider(db: &Db, provider_alias: &str) -> Result<Vec<String>> {
    let result = db.query_row(
        "SELECT value FROM kv WHERE scope = ? AND key = ?",
        &[&SCOPE, &provider_alias],
        |row| {
            let value: String = row.get(0)?;
            Ok(value)
        },
    )?;

    match result {
        Some(value) => {
            let parsed = parse_json(Some(value), json!([]));
            Ok(parsed
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default())
        }
        None => Ok(vec![]),
    }
}

/// Disable models for a provider (merge with existing disabled list)
pub fn disable_models(db: &Db, provider_alias: &str, ids: &[String]) -> Result<()> {
    if provider_alias.is_empty() || ids.is_empty() {
        return Ok(());
    }

    // TODO(dedupe): Implement atomic read-modify-write (requires transaction)
    let existing = get_disabled_by_provider(db, provider_alias)?;
    let mut merged_set: HashSet<String> = existing.into_iter().collect();
    for id in ids {
        merged_set.insert(id.clone());
    }
    let merged: Vec<String> = merged_set.into_iter().collect();

    let merged_json = stringify_json(&serde_json::to_value(&merged)?);
    db.execute_with_params(
        "INSERT INTO kv(scope, key, value) VALUES(?, ?, ?) ON CONFLICT(scope, key) DO UPDATE SET value = excluded.value",
        [SCOPE.to_string(), provider_alias.to_string(), merged_json],
    )?;

    Ok(())
}

/// Enable models for a provider (remove from disabled list)
pub fn enable_models(db: &Db, provider_alias: &str, ids: &[String]) -> Result<()> {
    if provider_alias.is_empty() {
        return Ok(());
    }

    // TODO(dedupe): Implement atomic read-modify-write (requires transaction)
    if ids.is_empty() {
        // Delete the entire entry if no IDs specified
        db.execute_with_params(
            "DELETE FROM kv WHERE scope = ? AND key = ?",
            [SCOPE.to_string(), provider_alias.to_string()],
        )?;
        return Ok(());
    }

    let existing = get_disabled_by_provider(db, provider_alias)?;
    let ids_to_remove: HashSet<String> = ids.iter().cloned().collect();
    let next: Vec<String> = existing
        .into_iter()
        .filter(|id| !ids_to_remove.contains(id))
        .collect();

    if next.is_empty() {
        db.execute_with_params(
            "DELETE FROM kv WHERE scope = ? AND key = ?",
            [SCOPE.to_string(), provider_alias.to_string()],
        )?;
    } else {
        let next_json = stringify_json(&serde_json::to_value(&next)?);
        db.execute_with_params(
            "INSERT INTO kv(scope, key, value) VALUES(?, ?, ?) ON CONFLICT(scope, key) DO UPDATE SET value = excluded.value",
            [SCOPE.to_string(), provider_alias.to_string(), next_json],
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_disable_and_get_models() {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();

        let ids = vec!["model1".to_string(), "model2".to_string()];
        disable_models(&db, "openai", &ids).unwrap();

        let disabled = get_disabled_by_provider(&db, "openai").unwrap();
        assert_eq!(disabled.len(), 2);
        assert!(disabled.contains(&"model1".to_string()));
        assert!(disabled.contains(&"model2".to_string()));
    }

    #[test]
    fn test_enable_models() {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();

        let ids = vec!["model1".to_string(), "model2".to_string()];
        disable_models(&db, "openai", &ids).unwrap();

        let to_enable = vec!["model1".to_string()];
        enable_models(&db, "openai", &to_enable).unwrap();

        let disabled = get_disabled_by_provider(&db, "openai").unwrap();
        assert_eq!(disabled.len(), 1);
        assert_eq!(disabled[0], "model2");
    }
}
