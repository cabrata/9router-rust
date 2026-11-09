use crate::connection::Db;
use crate::helpers::{parse_json, stringify_json};
use nr_core::Result;
use serde_json::{json, Value};

const PRICING_SCOPE: &str = "pricing";

/// Get all pricing (user overrides merged with defaults)
pub fn get_pricing(db: &Db) -> Result<Value> {
    let rows = db.query_all(
        "SELECT key, value FROM kv WHERE scope = ?",
        &[&PRICING_SCOPE],
        |row| {
            let key: String = row.get(0)?;
            let value: String = row.get(1)?;
            Ok((key, value))
        },
    )?;

    let mut out = serde_json::Map::new();
    for (provider, value_str) in rows {
        let pricing_data = parse_json(Some(value_str), json!({}));
        out.insert(provider, pricing_data);
    }

    // TODO(dedupe): Merge with default pricing from nr-data once available
    Ok(Value::Object(out))
}

/// Get pricing for a specific model
pub fn get_pricing_for_model(db: &Db, provider: &str, model: &str) -> Result<Option<Value>> {
    if model.is_empty() {
        return Ok(None);
    }

    let result = db.query_row(
        "SELECT value FROM kv WHERE scope = ? AND key = ?",
        &[&PRICING_SCOPE, &provider],
        |row| {
            let value_str: String = row.get(0)?;
            Ok(parse_json(Some(value_str), json!({})))
        },
    )?;

    match result {
        Some(provider_pricing) => {
            if let Some(model_pricing) = provider_pricing.get(model) {
                return Ok(Some(model_pricing.clone()));
            }
        }
        None => {}
    }

    // TODO(dedupe): Check default pricing from nr-data once available
    Ok(None)
}

/// Update pricing for providers/models (returns updated user pricing)
pub fn update_pricing(db: &Db, pricing_data: &Value) -> Result<Value> {
    db.transaction(|db| {
        if let Some(obj) = pricing_data.as_object() {
            for (provider, models) in obj {
                if let Some(model_map) = models.as_object() {
                    // For each provider, merge with existing pricing
                    let existing = db
                        .query_row(
                            "SELECT value FROM kv WHERE scope = ? AND key = ?",
                            &[&PRICING_SCOPE as &dyn rusqlite::ToSql, &provider],
                            |row| {
                                let value_str: String = row.get(0)?;
                                Ok(parse_json(Some(value_str), json!({})))
                            },
                        )?
                        .unwrap_or_else(|| json!({}));

                    let mut merged = existing.as_object().cloned().unwrap_or_default();
                    for (model, pricing) in model_map {
                        merged.insert(model.clone(), pricing.clone());
                    }

                    let merged_json = stringify_json(&Value::Object(merged));
                    db.execute_with_params(
                        "INSERT INTO kv(scope, key, value) VALUES(?, ?, ?) ON CONFLICT(scope, key) DO UPDATE SET value = excluded.value",
                        (PRICING_SCOPE, provider, &merged_json),
                    )?;
                }
            }
        }
        Ok(())
    })?;

    // Return updated user pricing
    get_pricing(db)
}

/// Reset pricing for a provider or model (returns remaining user pricing)
pub fn reset_pricing(db: &Db, provider: Option<&str>, model: Option<&str>) -> Result<Value> {
    if provider.is_none() {
        // Reset all pricing
        db.execute_with_params(
            "DELETE FROM kv WHERE scope = ?",
            [PRICING_SCOPE as &dyn rusqlite::ToSql],
        )?;
        return Ok(json!({}));
    }

    let provider_str = provider.unwrap();

    if let Some(m) = model {
        // Remove specific model from provider
        let existing = db.query_row(
            "SELECT value FROM kv WHERE scope = ? AND key = ?",
            &[&PRICING_SCOPE as &dyn rusqlite::ToSql, &provider_str],
            |row| {
                let value_str: String = row.get(0)?;
                Ok(parse_json(Some(value_str), json!({})))
            },
        )?;

        if let Some(mut provider_pricing) = existing.and_then(|v| v.as_object().cloned()) {
            provider_pricing.remove(m);

            if provider_pricing.is_empty() {
                db.execute_with_params(
                    "DELETE FROM kv WHERE scope = ? AND key = ?",
                    (PRICING_SCOPE, provider_str),
                )?;
            } else {
                let pricing_json = stringify_json(&Value::Object(provider_pricing));
                db.execute_with_params(
                    "INSERT INTO kv(scope, key, value) VALUES(?, ?, ?) ON CONFLICT(scope, key) DO UPDATE SET value = excluded.value",
                    (PRICING_SCOPE, provider_str, &pricing_json),
                )?;
            }
        }
    } else {
        // Remove all pricing for provider
        db.execute_with_params(
            "DELETE FROM kv WHERE scope = ? AND key = ?",
            (PRICING_SCOPE, provider_str),
        )?;
    }

    // Return remaining user pricing
    get_pricing(db)
}

/// Reset all user pricing (delete all overrides)
pub fn reset_all_pricing(db: &Db) -> Result<Value> {
    db.execute_with_params(
        "DELETE FROM kv WHERE scope = ?",
        [PRICING_SCOPE as &dyn rusqlite::ToSql],
    )?;
    Ok(json!({}))
}
