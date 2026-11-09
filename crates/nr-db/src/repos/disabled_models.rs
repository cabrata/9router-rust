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

/// Disable models for a provider (merge with existing disabled list).
/// Atomic read-merge-write inside a transaction, like disableModels().
pub fn disable_models(db: &Db, provider_alias: &str, ids: &[String]) -> Result<()> {
    if provider_alias.is_empty() || ids.is_empty() {
        return Ok(());
    }

    db.with_tx(|tx| {
        let existing = read_disabled(tx, provider_alias)?;
        // [...new Set([...current, ...ids])] - insertion order preserved for the
        // existing entries, new ids appended.
        let mut seen: HashSet<String> = existing.iter().cloned().collect();
        let mut merged = existing;
        for id in ids {
            if seen.insert(id.clone()) {
                merged.push(id.clone());
            }
        }

        let merged_json = stringify_json(&serde_json::to_value(&merged)?);
        tx.execute_with_params(
            "INSERT INTO kv(scope, key, value) VALUES(?, ?, ?) ON CONFLICT(scope, key) DO UPDATE SET value = excluded.value",
            [SCOPE, provider_alias, merged_json.as_str()],
        )?;
        Ok(())
    })
}

/// Enable models for a provider (remove from disabled list).
/// Atomic read-modify-write inside a transaction, like enableModels().
pub fn enable_models(db: &Db, provider_alias: &str, ids: &[String]) -> Result<()> {
    if provider_alias.is_empty() {
        return Ok(());
    }

    db.with_tx(|tx| {
        if ids.is_empty() {
            // No IDs specified: drop the whole provider entry.
            tx.execute_with_params(
                "DELETE FROM kv WHERE scope = ? AND key = ?",
                [SCOPE, provider_alias],
            )?;
            return Ok(());
        }

        let existing = read_disabled(tx, provider_alias)?;
        let remove: HashSet<&String> = ids.iter().collect();
        let next: Vec<String> = existing.into_iter().filter(|id| !remove.contains(id)).collect();

        if next.is_empty() {
            tx.execute_with_params(
                "DELETE FROM kv WHERE scope = ? AND key = ?",
                [SCOPE, provider_alias],
            )?;
        } else {
            let next_json = stringify_json(&serde_json::to_value(&next)?);
            tx.execute_with_params(
                "INSERT INTO kv(scope, key, value) VALUES(?, ?, ?) ON CONFLICT(scope, key) DO UPDATE SET value = excluded.value",
                [SCOPE, provider_alias, next_json.as_str()],
            )?;
        }
        Ok(())
    })
}

/// Read one provider's disabled list inside an open transaction.
fn read_disabled(tx: &crate::connection::DbTx, provider_alias: &str) -> Result<Vec<String>> {
    let value = tx.query_row(
        "SELECT value FROM kv WHERE scope = ? AND key = ?",
        &[&SCOPE, &provider_alias],
        |row| row.get::<_, String>(0),
    )?;
    Ok(match value {
        Some(value) => parse_json(Some(value), json!([]))
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        None => Vec::new(),
    })
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
    fn test_disable_merges_and_dedupes_preserving_order() {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();

        disable_models(&db, "openai", &["a".to_string(), "b".to_string()]).unwrap();
        // Re-disable an existing id plus a new one: order is kept, no duplicates.
        disable_models(&db, "openai", &["b".to_string(), "c".to_string()]).unwrap();

        let stored: Vec<String> = db
            .query_row(
                "SELECT value FROM kv WHERE scope = 'disabledModels' AND key = 'openai'",
                &[],
                |row| row.get(0),
            )
            .unwrap()
            .map(|raw: String| serde_json::from_str(&raw).unwrap())
            .unwrap();
        assert_eq!(stored, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
    }

    #[test]
    fn test_enable_all_drops_entry_and_get_all_reflects_it() {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();

        disable_models(&db, "openai", &["a".to_string()]).unwrap();
        disable_models(&db, "anthropic", &["b".to_string()]).unwrap();
        let all = get_disabled_models(&db).unwrap();
        assert_eq!(all["openai"], json!(["a"]));

        // Enabling with no ids clears the provider entry.
        enable_models(&db, "openai", &[]).unwrap();
        let all = get_disabled_models(&db).unwrap();
        assert!(!all.as_object().unwrap().contains_key("openai"));
        assert_eq!(all["anthropic"], json!(["b"]));

        // Enabling every remaining id also removes the row.
        enable_models(&db, "anthropic", &["b".to_string()]).unwrap();
        assert!(get_disabled_models(&db).unwrap().as_object().unwrap().is_empty());
        assert!(get_disabled_by_provider(&db, "anthropic").unwrap().is_empty());
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
