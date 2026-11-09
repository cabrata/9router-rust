use crate::connection::Db;
use crate::helpers::{parse_json, stringify_json};
use nr_core::Result;
use serde_json::{json, Value};

const MODEL_ALIASES_SCOPE: &str = "modelAliases";
const CUSTOM_MODELS_SCOPE: &str = "customModels";
const MITM_ALIAS_SCOPE: &str = "mitmAlias";

// ─── Model Aliases ──────────────────────────────────────────────────────────

/// Get all model aliases
pub fn get_model_aliases(db: &Db) -> Result<Value> {
    let rows = db.query_all(
        "SELECT key, value FROM kv WHERE scope = ?",
        &[&MODEL_ALIASES_SCOPE],
        |row| {
            let key: String = row.get(0)?;
            let value: String = row.get(1)?;
            Ok((key, value))
        },
    )?;

    let mut out = serde_json::Map::new();
    for (key, value) in rows {
        out.insert(key, Value::String(value));
    }

    Ok(Value::Object(out))
}

/// Set a model alias
pub fn set_model_alias(db: &Db, alias: &str, model: &str) -> Result<()> {
    db.execute_with_params(
        "INSERT INTO kv(scope, key, value) VALUES(?, ?, ?) ON CONFLICT(scope, key) DO UPDATE SET value = excluded.value",
        [MODEL_ALIASES_SCOPE.to_string(), alias.to_string(), model.to_string()],
    )?;
    Ok(())
}

/// Delete a model alias
pub fn delete_model_alias(db: &Db, alias: &str) -> Result<()> {
    db.execute_with_params(
        "DELETE FROM kv WHERE scope = ? AND key = ?",
        [MODEL_ALIASES_SCOPE.to_string(), alias.to_string()],
    )?;
    Ok(())
}

// ─── Custom Models ──────────────────────────────────────────────────────────

/// Get all custom models
pub fn get_custom_models(db: &Db) -> Result<Vec<Value>> {
    let rows = db.query_all(
        "SELECT value FROM kv WHERE scope = ?",
        &[&CUSTOM_MODELS_SCOPE],
        |row| {
            let value: String = row.get(0)?;
            let parsed = parse_json(Some(value), json!({}));
            Ok(parsed)
        },
    )?;

    Ok(rows)
}

/// Helper to create a custom model key
fn custom_key(provider_alias: &str, id: &str, model_type: &str) -> String {
    format!("{}|{}|{}", provider_alias, id, model_type)
}

/// Add or update a custom model
pub fn add_custom_model(
    db: &Db,
    provider_alias: &str,
    id: &str,
    model_type: Option<&str>,
    name: Option<&str>,
    caps: Option<&Value>,
    transport: Option<&str>,
) -> Result<bool> {
    let model_type = model_type.unwrap_or("llm");
    let key = custom_key(provider_alias, id, model_type);

    // TODO(dedupe): Implement atomic read-modify-write (requires transaction)
    let existing = db.query_row(
        "SELECT value FROM kv WHERE scope = ? AND key = ?",
        &[&CUSTOM_MODELS_SCOPE, &key],
        |row| {
            let value: String = row.get(0)?;
            Ok(parse_json(Some(value), json!({})))
        },
    )?;

    let mut added = false;
    if let Some(prev) = existing {
        // Update existing
        let mut next = prev.clone();
        if let Some(n) = name {
            next["name"] = Value::String(n.to_string());
        }
        if let Some(c) = caps {
            next["caps"] = c.clone();
        }
        if let Some(t) = transport {
            next["transport"] = Value::String(t.to_string());
        }

        let next_json = stringify_json(&next);
        db.execute_with_params(
            "UPDATE kv SET value = ? WHERE scope = ? AND key = ?",
            [next_json, CUSTOM_MODELS_SCOPE.to_string(), key],
        )?;
    } else {
        // Insert new
        let mut value = json!({
            "providerAlias": provider_alias,
            "id": id,
            "type": model_type,
            "name": name.unwrap_or(id),
        });

        if let Some(c) = caps {
            value["caps"] = c.clone();
        }
        if let Some(t) = transport {
            value["transport"] = Value::String(t.to_string());
        }

        let value_json = stringify_json(&value);
        db.execute_with_params(
            "INSERT INTO kv(scope, key, value) VALUES(?, ?, ?)",
            [CUSTOM_MODELS_SCOPE.to_string(), key, value_json],
        )?;

        added = true;
    }

    Ok(added)
}

/// Delete a custom model
pub fn delete_custom_model(
    db: &Db,
    provider_alias: &str,
    id: &str,
    model_type: Option<&str>,
) -> Result<()> {
    let model_type = model_type.unwrap_or("llm");
    let key = custom_key(provider_alias, id, model_type);

    db.execute_with_params(
        "DELETE FROM kv WHERE scope = ? AND key = ?",
        [CUSTOM_MODELS_SCOPE.to_string(), key],
    )?;

    Ok(())
}

// ─── MITM Aliases ───────────────────────────────────────────────────────────

/// Get all MITM aliases or one specific tool's aliases
pub fn get_mitm_alias(db: &Db, tool_name: Option<&str>) -> Result<Value> {
    if let Some(tool) = tool_name {
        let result = db.query_row(
            "SELECT value FROM kv WHERE scope = ? AND key = ?",
            &[&MITM_ALIAS_SCOPE, &tool],
            |row| {
                let value: String = row.get(0)?;
                Ok(parse_json(Some(value), json!({})))
            },
        )?;

        match result {
            Some(value) => Ok(value),
            None => Ok(json!({})),
        }
    } else {
        let rows = db.query_all(
            "SELECT key, value FROM kv WHERE scope = ?",
            &[&MITM_ALIAS_SCOPE],
            |row| {
                let key: String = row.get(0)?;
                let value: String = row.get(1)?;
                Ok((key, value))
            },
        )?;

        let mut out = serde_json::Map::new();
        for (key, value) in rows {
            let parsed = parse_json(Some(value), json!({}));
            out.insert(key, parsed);
        }

        Ok(Value::Object(out))
    }
}

/// Set MITM alias mappings for a tool
pub fn set_mitm_alias_all(db: &Db, tool_name: &str, mappings: &Value) -> Result<()> {
    let mappings_json = stringify_json(mappings);
    db.execute_with_params(
        "INSERT INTO kv(scope, key, value) VALUES(?, ?, ?) ON CONFLICT(scope, key) DO UPDATE SET value = excluded.value",
        [MITM_ALIAS_SCOPE.to_string(), tool_name.to_string(), mappings_json],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_model_aliases() {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();

        set_model_alias(&db, "gpt4", "openai:gpt-4").unwrap();
        let aliases = get_model_aliases(&db).unwrap();
        assert_eq!(aliases.get("gpt4").and_then(|v| v.as_str()), Some("openai:gpt-4"));

        delete_model_alias(&db, "gpt4").unwrap();
        let aliases = get_model_aliases(&db).unwrap();
        assert!(!aliases.as_object().unwrap().contains_key("gpt4"));
    }

    #[test]
    fn test_custom_models() {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();

        let added = add_custom_model(&db, "openai", "gpt-5", None, Some("GPT-5"), None, None).unwrap();
        assert!(added);

        let models = get_custom_models(&db).unwrap();
        assert!(!models.is_empty());

        delete_custom_model(&db, "openai", "gpt-5", None).unwrap();
        let models = get_custom_models(&db).unwrap();
        assert!(models.is_empty());
    }
}
