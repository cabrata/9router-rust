use axum::extract::{Extension, Path, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use nr_db::{get_combo_by_id, get_combo_by_name, get_combos, update_combo, Combo, Db};
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::LazyLock;

use super::common::{bad_request, get_db, internal_error, not_found};

static VALID_NAME_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9_.\-]+$").expect("valid regex"));

fn combo_to_json(c: &Combo) -> Value {
    json!({
        "id": c.id,
        "name": c.name,
        "kind": c.kind,
        "models": c.models,
        "createdAt": c.created_at,
        "updatedAt": c.updated_at,
    })
}

// GET /api/combos - Get all combos
pub async fn get_combos_handler(ext: Option<Extension<Db>>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    match get_combos(&db) {
        Ok(combos) => {
            let json_combos: Vec<Value> = combos.iter().map(combo_to_json).collect();
            (StatusCode::OK, Json(json!({ "combos": json_combos }))).into_response()
        }
        Err(e) => internal_error(format!("Failed to fetch combos: {e}")),
    }
}

// POST /api/combos - Create combo
pub async fn post_combos(ext: Option<Extension<Db>>, Json(body): Json<Value>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let name = match body.get("name").and_then(|v| v.as_str()) {
        Some(n) if !n.trim().is_empty() => n.trim(),
        _ => return bad_request("Name is required"),
    };

    if !VALID_NAME_REGEX.is_match(name) {
        return bad_request("Name can only contain letters, numbers, -, _ and .");
    }

    match get_combo_by_name(&db, name) {
        Ok(Some(_)) => return bad_request("Combo name already exists"),
        Ok(None) => {}
        Err(e) => return internal_error(format!("Database error: {e}")),
    }

    let create_payload = json!({
        "name": name,
        "models": body.get("models").cloned().unwrap_or_else(|| json!([])),
        "kind": body.get("kind").cloned().unwrap_or(Value::Null),
    });

    match nr_db::create_combo(&db, &create_payload) {
        Ok(combo) => (StatusCode::CREATED, Json(combo_to_json(&combo))).into_response(),
        Err(e) => internal_error(format!("Failed to create combo: {e}")),
    }
}

// GET /api/combos/:id - Get combo by ID
pub async fn get_combo_by_id_handler(
    ext: Option<Extension<Db>>,
    Path(id): Path<String>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    match get_combo_by_id(&db, &id) {
        Ok(Some(combo)) => (StatusCode::OK, Json(combo_to_json(&combo))).into_response(),
        Ok(None) => not_found("Combo not found"),
        Err(e) => internal_error(format!("Failed to fetch combo: {e}")),
    }
}

// PUT /api/combos/:id - Update combo
pub async fn put_combo_by_id(
    ext: Option<Extension<Db>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    if let Some(name) = body.get("name").and_then(|v| v.as_str()) {
        if !VALID_NAME_REGEX.is_match(name) {
            return bad_request("Name can only contain letters, numbers, -, _ and .");
        }

        match get_combo_by_name(&db, name) {
            Ok(Some(existing)) if existing.id != id => {
                return bad_request("Combo name already exists");
            }
            Ok(_) => {}
            Err(e) => return internal_error(format!("Database error: {e}")),
        }
    }

    match update_combo(&db, &id, &body) {
        Ok(Some(updated)) => (StatusCode::OK, Json(combo_to_json(&updated))).into_response(),
        Ok(None) => not_found("Combo not found"),
        Err(e) => internal_error(format!("Failed to update combo: {e}")),
    }
}

// DELETE /api/combos/:id - Delete combo
pub async fn delete_combo_by_id(
    ext: Option<Extension<Db>>,
    Path(id): Path<String>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    match nr_db::delete_combo(&db, &id) {
        Ok(true) => (StatusCode::OK, Json(json!({ "success": true }))).into_response(),
        Ok(false) => not_found("Combo not found"),
        Err(e) => internal_error(format!("Failed to delete combo: {e}")),
    }
}

#[derive(Deserialize)]
pub struct PresetQuery {
    pub source: Option<String>,
}

#[derive(serde::Serialize, Clone)]
pub struct PresetItem {
    pub name: String,
    pub models: Vec<String>,
    pub exists: bool,
}

fn build_preset_items(source: &str, existing_names: &HashSet<String>) -> Vec<PresetItem> {
    let mut items = Vec::new();
    let mut seen = HashSet::new();

    let mut push_item = |name: String, models: Vec<String>| {
        if !VALID_NAME_REGEX.is_match(&name) || seen.contains(&name) || models.is_empty() {
            return;
        }
        seen.insert(name.clone());
        let exists = existing_names.contains(&name);
        items.push(PresetItem { name, models, exists });
    };

    if source == "cursor" {
        let cursor_models = nr_data::get_provider_models("cu");
        for model in cursor_models {
            if let Some(id) = model.get("id").and_then(|v| v.as_str()) {
                push_item(id.to_string(), vec![format!("cu/{id}")]);
            }
        }
    } else if source == "claude" {
        let claude_models = nr_data::get_provider_models("cc");
        for model in claude_models {
            if let Some(id) = model.get("id").and_then(|v| v.as_str()) {
                push_item(id.to_string(), vec![format!("cc/{id}")]);
            }
        }

        // Claude extra targets from src/lib/comboPresets.js
        push_item("default".to_string(), vec!["cc/claude-sonnet-5".to_string()]);
        push_item("opusplan".to_string(), vec!["cc/claude-opus-5".to_string()]);
    }

    items
}

// GET /api/combos/presets - Preview preset items
pub async fn get_combos_presets(
    ext: Option<Extension<Db>>,
    Query(query): Query<PresetQuery>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let source = match query.source.as_deref() {
        Some("cursor") => "cursor",
        Some("claude") => "claude",
        _ => return bad_request("source must be 'cursor' or 'claude'"),
    };

    let existing = match get_combos(&db) {
        Ok(c) => c.into_iter().map(|item| item.name).collect::<HashSet<_>>(),
        Err(e) => return internal_error(format!("Failed to fetch combos: {e}")),
    };

    let items = build_preset_items(source, &existing);
    let to_create = items.iter().filter(|i| !i.exists).count();
    let to_skip = items.iter().filter(|i| i.exists).count();

    (
        StatusCode::OK,
        Json(json!({
            "source": source,
            "items": items,
            "toCreate": to_create,
            "toSkip": to_skip,
        })),
    )
        .into_response()
}

// POST /api/combos/presets - Create missing presets
pub async fn post_combos_presets(
    ext: Option<Extension<Db>>,
    Json(body): Json<Value>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let source = match body.get("source").and_then(|v| v.as_str()) {
        Some("cursor") => "cursor",
        Some("claude") => "claude",
        _ => return bad_request("source must be 'cursor' or 'claude'"),
    };

    let existing = match get_combos(&db) {
        Ok(c) => c.into_iter().map(|item| item.name).collect::<HashSet<_>>(),
        Err(e) => return internal_error(format!("Failed to fetch combos: {e}")),
    };

    let items = build_preset_items(source, &existing);
    let mut created = Vec::new();
    let mut skipped = Vec::new();

    for item in items {
        if item.exists {
            skipped.push(item.name);
            continue;
        }

        let create_data = json!({
            "name": item.name,
            "models": item.models,
        });

        match nr_db::create_combo(&db, &create_data) {
            Ok(c) => created.push(combo_to_json(&c)),
            Err(e) => {
                tracing::warn!("Failed to create combo preset {}: {e}", item.name);
                skipped.push(item.name);
            }
        }
    }

    let created_count = created.len();
    let skipped_count = skipped.len();

    (
        StatusCode::OK,
        Json(json!({
            "source": source,
            "created": created,
            "skipped": skipped,
            "createdCount": created_count,
            "skippedCount": skipped_count,
        })),
    )
        .into_response()
}
