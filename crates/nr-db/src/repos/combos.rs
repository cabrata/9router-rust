use crate::connection::Db;
use crate::helpers::{parse_json, stringify_json};
use nr_core::Result;
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct Combo {
    pub id: String,
    pub name: String,
    pub kind: Option<String>,
    pub models: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

fn row_to_combo(
    id: String,
    name: String,
    kind: Option<String>,
    models: String,
    created_at: String,
    updated_at: String,
) -> Combo {
    Combo {
        id,
        name,
        kind,
        models: parse_json(Some(models), json!([]))
            .as_array()
            .map(|v| {
                v.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default(),
        created_at,
        updated_at,
    }
}

/// Get all combos ordered by creation time
pub fn get_combos(db: &Db) -> Result<Vec<Combo>> {
    db.query_all(
        "SELECT id, name, kind, models, createdAt, updatedAt FROM combos ORDER BY createdAt ASC",
        &[],
        |row| {
            Ok(row_to_combo(
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        },
    )
}

/// Get a specific combo by ID
pub fn get_combo_by_id(db: &Db, id: &str) -> Result<Option<Combo>> {
    db.query_row(
        "SELECT id, name, kind, models, createdAt, updatedAt FROM combos WHERE id = ?",
        &[&id],
        |row| {
            Ok(row_to_combo(
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        },
    )
}

/// Get a specific combo by name
pub fn get_combo_by_name(db: &Db, name: &str) -> Result<Option<Combo>> {
    db.query_row(
        "SELECT id, name, kind, models, createdAt, updatedAt FROM combos WHERE name = ?",
        &[&name],
        |row| {
            Ok(row_to_combo(
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        },
    )
}

/// Create a new combo
pub fn create_combo(db: &Db, data: &Value) -> Result<Combo> {
    let id = Uuid::new_v4().to_string();
    let name = data
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| nr_core::Error::other("name is required"))?
        .to_string();
    let kind = data.get("kind").and_then(|v| v.as_str()).map(|s| s.to_string());
    let models = data.get("models").cloned().unwrap_or(json!([]));
    let now = chrono::Utc::now().to_rfc3339();

    db.execute_with_params(
        "INSERT INTO combos(id, name, kind, models, createdAt, updatedAt) VALUES(?, ?, ?, ?, ?, ?)",
        [
            id.clone(),
            name.clone(),
            kind.clone().unwrap_or_default(),
            stringify_json(&models),
            now.clone(),
            now.clone(),
        ],
    )?;

    Ok(Combo {
        id,
        name,
        kind,
        models: models
            .as_array()
            .map(|v| {
                v.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default(),
        created_at: now.clone(),
        updated_at: now,
    })
}

/// Update a combo
pub fn update_combo(db: &Db, id: &str, updates: &Value) -> Result<Option<Combo>> {
    let existing = get_combo_by_id(db, id)?;
    match existing {
        None => Ok(None),
        Some(mut combo) => {
            if let Some(name) = updates.get("name").and_then(|v| v.as_str()) {
                combo.name = name.to_string();
            }
            if let Some(kind) = updates.get("kind") {
                combo.kind = kind.as_str().map(|s| s.to_string());
            }
            if let Some(models) = updates.get("models") {
                if let Some(arr) = models.as_array() {
                    combo.models = arr
                        .iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect();
                }
            }
            combo.updated_at = chrono::Utc::now().to_rfc3339();

            let models_json = stringify_json(&serde_json::to_value(&combo.models)?);
            db.execute_with_params(
                "UPDATE combos SET name = ?, kind = ?, models = ?, updatedAt = ? WHERE id = ?",
                [
                    combo.name.clone(),
                    combo.kind.clone().unwrap_or_default(),
                    models_json,
                    combo.updated_at.clone(),
                    id.to_string(),
                ],
            )?;

            Ok(Some(combo))
        }
    }
}

/// Delete a combo
pub fn delete_combo(db: &Db, id: &str) -> Result<bool> {
    let rows = db.execute_with_params("DELETE FROM combos WHERE id = ?", [id])?;
    Ok(rows > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_create_and_get_combo() {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        let data = json!({"name": "test-combo", "models": ["model1", "model2"]});
        let combo = create_combo(&db, &data).unwrap();
        assert_eq!(combo.name, "test-combo");
        assert_eq!(combo.models, vec!["model1", "model2"]);

        let retrieved = get_combo_by_id(&db, &combo.id).unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.name, "test-combo");
    }

    #[test]
    fn test_get_combo_by_name() {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        let data = json!({"name": "named-combo", "models": ["model1"]});
        create_combo(&db, &data).unwrap();

        let retrieved = get_combo_by_name(&db, "named-combo").unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().name, "named-combo");
    }

    #[test]
    fn test_delete_combo() {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        let data = json!({"name": "to-delete", "models": ["model1"]});
        let combo = create_combo(&db, &data).unwrap();

        let deleted = delete_combo(&db, &combo.id).unwrap();
        assert!(deleted);

        let retrieved = get_combo_by_id(&db, &combo.id).unwrap();
        assert!(retrieved.is_none());
    }
}
