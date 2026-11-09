use crate::connection::Db;
use crate::helpers::{parse_json, stringify_json};
use nr_core::Result;
use serde_json::{json, Map, Value};

const PRICING_SCOPE: &str = "pricing";

/// Raw user overrides (kv scope `pricing`) as `{provider: {model: pricing}}`.
fn get_user_pricing(db: &Db) -> Result<Map<String, Value>> {
    let rows = db.query_all(
        "SELECT key, value FROM kv WHERE scope = ?",
        &[&PRICING_SCOPE],
        |row| {
            let key: String = row.get(0)?;
            let value: String = row.get(1)?;
            Ok((key, parse_json(Some(value), json!({}))))
        },
    )?;

    let mut out = Map::new();
    for (provider, value) in rows {
        out.insert(provider, value);
    }
    Ok(out)
}

/// `getPricing()`: defaults from `open-sse/providers/pricing.js` PROVIDER_PRICING
/// merged with user overrides, user values winning per model field.
pub fn get_pricing(db: &Db) -> Result<Value> {
    let user_pricing = get_user_pricing(db)?;

    let defaults = nr_data::pricing::get_default_pricing();
    let mut merged: Map<String, Value> = Map::new();

    if let Some(defaults_obj) = defaults.as_object() {
        for (provider, models) in defaults_obj {
            let merged_models = merged
                .entry(provider.clone())
                .or_insert_with(|| json!({}));
            if let Some(dst) = merged_models.as_object_mut() {
                if let Some(src) = models.as_object() {
                    for (model, pricing) in src {
                        dst.insert(model.clone(), pricing.clone());
                    }
                }
                // user overrides win, merged per field when both exist
                if let Some(user_models) = user_pricing.get(provider).and_then(Value::as_object) {
                    for (model, pricing) in user_models {
                        match dst.get_mut(model) {
                            Some(existing) if existing.is_object() && pricing.is_object() => {
                                if let (Some(e), Some(p)) =
                                    (existing.as_object_mut(), pricing.as_object())
                                {
                                    for (k, v) in p {
                                        e.insert(k.clone(), v.clone());
                                    }
                                }
                            }
                            _ => {
                                dst.insert(model.clone(), pricing.clone());
                            }
                        }
                    }
                }
            }
        }
    }

    // providers only present in user pricing
    for (provider, models) in &user_pricing {
        if !merged.contains_key(provider) {
            merged.insert(provider.clone(), models.clone());
        } else if let Some(dst) = merged.get_mut(provider).and_then(Value::as_object_mut) {
            if let Some(src) = models.as_object() {
                for (model, pricing) in src {
                    if !dst.contains_key(model) {
                        dst.insert(model.clone(), pricing.clone());
                    }
                }
            }
        }
    }

    Ok(Value::Object(merged))
}

/// `getPricingForModel()`: user override first, then the built-in resolver.
pub fn get_pricing_for_model(db: &Db, provider: &str, model: &str) -> Result<Option<Value>> {
    if model.is_empty() {
        return Ok(None);
    }

    if !provider.is_empty() {
        let result = db.query_row(
            "SELECT value FROM kv WHERE scope = ? AND key = ?",
            &[&PRICING_SCOPE, &provider],
            |row| {
                let value_str: String = row.get(0)?;
                Ok(parse_json(Some(value_str), json!({})))
            },
        )?;

        if let Some(model_pricing) = result
            .as_ref()
            .and_then(|v| v.get(model))
            .filter(|v| !v.is_null())
        {
            return Ok(Some(model_pricing.clone()));
        }
    }

    Ok(nr_data::pricing::get_pricing_for_model(
        Some(provider),
        Some(model),
    ))
}

/// `updatePricing()`: per-provider read-modify-write, returns user overrides.
pub fn update_pricing(db: &Db, pricing_data: &Value) -> Result<Value> {
    db.with_tx(|tx| {
        if let Some(obj) = pricing_data.as_object() {
            for (provider, models) in obj {
                let model_map = match models.as_object() {
                    Some(m) => m,
                    None => continue,
                };
                let existing = tx
                    .query_row(
                        "SELECT value FROM kv WHERE scope = 'pricing' AND key = ?",
                        &[&provider],
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
                tx.execute_with_params(
                    "INSERT INTO kv(scope, key, value) VALUES('pricing', ?, ?) ON CONFLICT(scope, key) DO UPDATE SET value = excluded.value",
                    (&provider, &merged_json),
                )?;
            }
        }
        Ok(())
    })?;

    get_user_pricing(db).map(Value::Object)
}

/// `resetPricing()`: drop one model, one provider, or every override.
pub fn reset_pricing(db: &Db, provider: Option<&str>, model: Option<&str>) -> Result<Value> {
    let provider = match provider {
        None => return get_user_pricing(db).map(Value::Object),
        Some(p) if p.is_empty() => return get_user_pricing(db).map(Value::Object),
        Some(p) => p,
    };

    db.with_tx(|tx| {
        let model = match model {
            None => {
                tx.execute_with_params(
                    "DELETE FROM kv WHERE scope = 'pricing' AND key = ?",
                    [provider],
                )?;
                return Ok(());
            }
            Some(m) => m,
        };

        let existing = tx.query_row(
            "SELECT value FROM kv WHERE scope = 'pricing' AND key = ?",
            &[&provider],
            |row| {
                let value_str: String = row.get(0)?;
                Ok(parse_json(Some(value_str), json!({})))
            },
        )?;

        let mut current = existing
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        current.remove(model);

        if current.is_empty() {
            tx.execute_with_params(
                "DELETE FROM kv WHERE scope = 'pricing' AND key = ?",
                [provider],
            )?;
        } else {
            let current_json = stringify_json(&Value::Object(current));
            tx.execute_with_params(
                "INSERT INTO kv(scope, key, value) VALUES('pricing', ?, ?) ON CONFLICT(scope, key) DO UPDATE SET value = excluded.value",
                (provider, &current_json),
            )?;
        }
        Ok(())
    })?;

    get_user_pricing(db).map(Value::Object)
}

/// `resetAllPricing()`: clear every pricing override, return `{}`.
pub fn reset_all_pricing(db: &Db) -> Result<Value> {
    db.execute_with_params("DELETE FROM kv WHERE scope = 'pricing'", [])?;
    Ok(json!({}))
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
    fn pricing_roundtrip() {
        let (_dir, db) = db();

        // insert (update = merge into kv scope pricing)
        let updated = update_pricing(
            &db,
            &json!({"acme": {"acme-1": {"input": 1.0, "output": 2.0}}}),
        )
        .unwrap();
        assert_eq!(updated["acme"]["acme-1"]["input"], json!(1.0));

        // select
        let got = get_pricing_for_model(&db, "acme", "acme-1").unwrap().unwrap();
        assert_eq!(got["output"], json!(2.0));

        // merge keeps the other model
        update_pricing(&db, &json!({"acme": {"acme-2": {"input": 3.0}}})).unwrap();
        let user = get_user_pricing(&db).unwrap();
        assert!(user["acme"].get("acme-1").is_some());
        assert!(user["acme"].get("acme-2").is_some());

        // getPricing includes defaults and the override
        let all = get_pricing(&db).unwrap();
        assert!(all.get("acme").is_some());

        // update an existing model's field
        update_pricing(&db, &json!({"acme": {"acme-1": {"input": 9.0}}})).unwrap();
        let got = get_pricing_for_model(&db, "acme", "acme-1").unwrap().unwrap();
        assert_eq!(got["input"], json!(9.0));

        // delete one model
        reset_pricing(&db, Some("acme"), Some("acme-1")).unwrap();
        assert!(get_pricing_for_model(&db, "acme", "acme-1")
            .unwrap()
            .is_none()
            || get_pricing_for_model(&db, "acme", "acme-1").unwrap() != Some(got));

        // delete provider
        reset_pricing(&db, Some("acme"), None).unwrap();
        assert!(get_user_pricing(&db).unwrap().get("acme").is_none());

        // delete all
        update_pricing(&db, &json!({"zzz": {"m": {"input": 1.0}}})).unwrap();
        assert_eq!(reset_all_pricing(&db).unwrap(), json!({}));
        assert!(get_user_pricing(&db).unwrap().is_empty());
    }

    #[test]
    fn unknown_model_returns_none() {
        let (_dir, db) = db();
        assert!(get_pricing_for_model(&db, "nope", "nope-model").unwrap().is_none());
    }
}
