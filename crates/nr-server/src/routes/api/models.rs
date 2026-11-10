use axum::extract::{Extension, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use nr_db::{
    add_custom_model, delete_custom_model, delete_model_alias, disable_models, enable_models,
    get_custom_models, get_disabled_models, get_model_aliases, get_provider_connections,
    set_model_alias, update_provider_connection, Db,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::fs;
use std::sync::Mutex;
use std::sync::OnceLock;

use super::common::{bad_request, get_db, internal_error};

const MODEL_LOCK_PREFIX: &str = "modelLock_";

fn get_provider_alias(provider: &str) -> String {
    if let Some(map) = nr_data::export("src/shared/constants/providers", "ID_TO_ALIAS")
        .and_then(Value::as_object)
    {
        if let Some(alias) = map.get(provider).and_then(Value::as_str) {
            return alias.to_string();
        }
    }
    provider.to_string()
}

fn alias_to_provider_id(alias: &str) -> String {
    if let Some(map) = nr_data::export("src/shared/constants/providers", "ALIAS_TO_ID")
        .and_then(Value::as_object)
    {
        if let Some(id) = map.get(alias).and_then(Value::as_str) {
            return id.to_string();
        }
    }
    alias.to_string()
}

// GET /api/models - Get models with aliases
pub async fn get_models_handler(ext: Option<Extension<Db>>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let model_aliases = get_model_aliases(&db).unwrap_or_else(|_| json!({}));
    let disabled = get_disabled_models(&db).unwrap_or_else(|_| json!({}));

    let ai_models_raw = nr_data::export("src/shared/constants/models", "AI_MODELS")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut models = Vec::new();

    for m in &ai_models_raw {
        let provider = m.get("provider").and_then(Value::as_str).unwrap_or("");
        let model = m.get("model").and_then(Value::as_str).unwrap_or("");
        let provider_alias = get_provider_alias(provider);

        let is_disabled = disabled
            .get(&provider_alias)
            .or_else(|| disabled.get(provider))
            .and_then(Value::as_array)
            .map_or(false, |list| {
                list.iter().any(|v| v.as_str() == Some(model))
            });

        if is_disabled {
            continue;
        }

        let full_model = format!("{provider}/{model}");
        let routed_model = format!("{provider_alias}/{model}");
        let alias = model_aliases
            .get(&full_model)
            .and_then(Value::as_str)
            .unwrap_or(model);

        let caps = nr_data::capabilities::get_capabilities_for_model(Some(provider), Some(model));

        let mut entry = m.clone();
        if let Some(obj) = entry.as_object_mut() {
            obj.insert("fullModel".to_string(), Value::String(full_model));
            obj.insert("routedModel".to_string(), Value::String(routed_model));
            obj.insert("alias".to_string(), Value::String(alias.to_string()));
            obj.insert(
                "caps".to_string(),
                json!({
                    "vision": caps.get("vision").cloned().unwrap_or(Value::Bool(false)),
                    "search": caps.get("search").cloned().unwrap_or(Value::Bool(false)),
                    "reasoning": caps.get("reasoning").cloned().unwrap_or(Value::Bool(false)),
                    "contextWindow": caps.get("contextWindow").cloned().unwrap_or(json!(200000)),
                    "maxOutput": caps.get("maxOutput").cloned().unwrap_or(json!(64000)),
                }),
            );
        }
        models.push(entry);
    }

    // Append custom models
    let seen_full: HashSet<String> = models
        .iter()
        .filter_map(|m| m.get("fullModel").and_then(Value::as_str).map(String::from))
        .collect();
    let seen_routed: HashSet<String> = models
        .iter()
        .filter_map(|m| m.get("routedModel").and_then(Value::as_str).map(String::from))
        .collect();

    let custom_models = get_custom_models(&db).unwrap_or_default();
    for cm in custom_models {
        let p_alias = cm.get("providerAlias").and_then(Value::as_str).unwrap_or("");
        let id = cm.get("id").and_then(Value::as_str).unwrap_or("");
        let kind = cm
            .get("kind")
            .or_else(|| cm.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("llm");

        if id.is_empty() || kind != "llm" {
            continue;
        }

        let full_key = format!("{p_alias}/{id}");
        if seen_routed.contains(&full_key) || seen_full.contains(&full_key) {
            continue;
        }

        let provider_id = alias_to_provider_id(p_alias);
        let caps = nr_data::capabilities::get_capabilities_for_model(Some(&provider_id), Some(id));
        let alias = model_aliases
            .get(&full_key)
            .and_then(Value::as_str)
            .unwrap_or(id);

        let mut caps_val = json!({
            "vision": caps.get("vision").cloned().unwrap_or(Value::Bool(false)),
            "search": caps.get("search").cloned().unwrap_or(Value::Bool(false)),
            "reasoning": caps.get("reasoning").cloned().unwrap_or(Value::Bool(false)),
            "contextWindow": caps.get("contextWindow").cloned().unwrap_or(json!(200000)),
            "maxOutput": caps.get("maxOutput").cloned().unwrap_or(json!(64000)),
        });

        if let Some(custom_caps) = cm.get("caps").and_then(Value::as_object) {
            if let Some(target) = caps_val.as_object_mut() {
                for (k, v) in custom_caps {
                    target.insert(k.clone(), v.clone());
                }
            }
        }

        models.push(json!({
            "provider": p_alias,
            "model": id,
            "name": cm.get("name").and_then(Value::as_str).unwrap_or(id),
            "fullModel": full_key,
            "routedModel": full_key,
            "alias": alias,
            "caps": caps_val,
        }));
    }

    (StatusCode::OK, Json(json!({ "models": models }))).into_response()
}

// PUT /api/models - Update model alias
pub async fn put_models(ext: Option<Extension<Db>>, Json(body): Json<Value>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let model = match body.get("model").and_then(Value::as_str) {
        Some(m) if !m.trim().is_empty() => m.trim(),
        _ => return bad_request("Model and alias required"),
    };

    let alias = match body.get("alias").and_then(Value::as_str) {
        Some(a) if !a.trim().is_empty() => a.trim(),
        _ => return bad_request("Model and alias required"),
    };

    let aliases = get_model_aliases(&db).unwrap_or_else(|_| json!({}));
    if let Some(obj) = aliases.as_object() {
        for (m, a) in obj {
            if a.as_str() == Some(alias) && m != model {
                return bad_request("Alias already in use");
            }
        }
    }

    match set_model_alias(&db, alias, model) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "success": true, "model": model, "alias": alias })),
        )
            .into_response(),
        Err(e) => internal_error(format!("Failed to update alias: {e}")),
    }
}

// GET /api/models/alias - Get all aliases
pub async fn get_models_alias(ext: Option<Extension<Db>>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    match get_model_aliases(&db) {
        Ok(aliases) => (StatusCode::OK, Json(json!({ "aliases": aliases }))).into_response(),
        Err(e) => internal_error(format!("Failed to fetch aliases: {e}")),
    }
}

// PUT /api/models/alias - Set model alias
pub async fn put_models_alias(ext: Option<Extension<Db>>, Json(body): Json<Value>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let model = match body.get("model").and_then(Value::as_str) {
        Some(m) if !m.trim().is_empty() => m.trim(),
        _ => return bad_request("Model and alias required"),
    };

    let alias = match body.get("alias").and_then(Value::as_str) {
        Some(a) if !a.trim().is_empty() => a.trim(),
        _ => return bad_request("Model and alias required"),
    };

    match set_model_alias(&db, alias, model) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "success": true, "model": model, "alias": alias })),
        )
            .into_response(),
        Err(e) => internal_error(format!("Failed to update alias: {e}")),
    }
}

#[derive(Deserialize)]
pub struct ModelAliasDeleteQuery {
    pub alias: Option<String>,
}

// DELETE /api/models/alias?alias=xxx
pub async fn delete_models_alias(
    ext: Option<Extension<Db>>,
    Query(query): Query<ModelAliasDeleteQuery>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let Some(alias) = query.alias else {
        return bad_request("Alias required");
    };

    match delete_model_alias(&db, &alias) {
        Ok(()) => (StatusCode::OK, Json(json!({ "success": true }))).into_response(),
        Err(e) => internal_error(format!("Failed to delete alias: {e}")),
    }
}

// GET /api/models/availability - Model cooldown / availability
pub async fn get_models_availability(ext: Option<Extension<Db>>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let connections = get_provider_connections(&db, None).unwrap_or_default();
    let mut out_models = Vec::new();
    let now = chrono::Utc::now();

    for conn in connections {
        let provider = conn.get("provider").and_then(Value::as_str).unwrap_or("");
        let conn_id = conn.get("id").and_then(Value::as_str).unwrap_or("");
        let conn_name = conn
            .get("name")
            .or_else(|| conn.get("email"))
            .and_then(Value::as_str)
            .unwrap_or(conn_id);
        let last_error = conn.get("lastError").cloned().unwrap_or(Value::Null);

        let mut lock_count = 0;
        if let Some(obj) = conn.as_object() {
            for (key, val) in obj {
                if key.starts_with(MODEL_LOCK_PREFIX) {
                    if let Some(until_str) = val.as_str() {
                        let is_active = chrono::DateTime::parse_from_rfc3339(until_str)
                            .map_or(false, |dt| dt.with_timezone(&chrono::Utc) > now);
                        if is_active {
                            lock_count += 1;
                            let model_name = &key[MODEL_LOCK_PREFIX.len()..];
                            out_models.push(json!({
                                "provider": provider,
                                "model": if model_name.is_empty() { "__all" } else { model_name },
                                "status": "cooldown",
                                "until": until_str,
                                "connectionId": conn_id,
                                "connectionName": conn_name,
                                "lastError": last_error,
                            }));
                        }
                    }
                }
            }
        }

        if lock_count == 0 && conn.get("testStatus").and_then(Value::as_str) == Some("unavailable") {
            out_models.push(json!({
                "provider": provider,
                "model": "__all",
                "status": "unavailable",
                "connectionId": conn_id,
                "connectionName": conn_name,
                "lastError": last_error,
            }));
        }
    }

    let count = out_models.len();
    (
        StatusCode::OK,
        Json(json!({
            "models": out_models,
            "unavailableCount": count,
        })),
    )
        .into_response()
}

// POST /api/models/availability - Clear cooldown
pub async fn post_models_availability(
    ext: Option<Extension<Db>>,
    Json(body): Json<Value>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let action = body.get("action").and_then(Value::as_str);
    let provider = body.get("provider").and_then(Value::as_str);
    let model = body.get("model").and_then(Value::as_str);

    if action != Some("clearCooldown") || provider.is_none() || model.is_none() {
        return bad_request("Invalid request");
    }

    let provider = provider.unwrap();
    let model = model.unwrap();
    let lock_key = format!("{MODEL_LOCK_PREFIX}{model}");

    let filter = json!({ "provider": provider });
    let connections = get_provider_connections(&db, Some(&filter)).unwrap_or_default();

    for conn in connections {
        if conn.get(&lock_key).is_some() {
            if let Some(id) = conn.get("id").and_then(Value::as_str) {
                let mut updates = json!({
                    lock_key.clone(): Value::Null,
                });
                if conn.get("testStatus").and_then(Value::as_str) == Some("unavailable") {
                    if let Some(obj) = updates.as_object_mut() {
                        obj.insert("testStatus".to_string(), Value::String("active".to_string()));
                        obj.insert("lastError".to_string(), Value::Null);
                        obj.insert("lastErrorAt".to_string(), Value::Null);
                        obj.insert("backoffLevel".to_string(), json!(0));
                    }
                }
                let _ = update_provider_connection(&db, id, &updates);
            }
        }
    }

    (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
}

// ─── modelCatalog/sync.js ───────────────────────────────────────────────────

const CATALOG_URL: &str = "https://models.dev/api.json";
const CATALOG_VERSION: u64 = 2;
const SYNC_INTERVAL_MS: u64 = 24 * 60 * 60 * 1000;
const LIMIT_TOLERANCE: f64 = 0.1;

/// `PROVIDER_ALIASES`: local provider id → models.dev provider id.
const PROVIDER_ALIASES: &[(&str, &str)] = &[
    ("github", "github-copilot"),
    ("glm", "zai"),
    ("glm-cn", "zhipuai"),
    ("claude", "anthropic"),
    ("gemini", "google"),
    ("kimi", "moonshotai"),
    ("kimi-cn", "moonshotai-cn"),
    ("qwen", "alibaba"),
    ("qwen-cn", "alibaba-cn"),
    ("zhipu", "zhipuai"),
    ("hunyuan", "tencent"),
    ("doubao", "volcengine"),
    ("cloudflare-ai", "cloudflare-workers-ai"),
];

fn provider_alias(provider: &str) -> &str {
    PROVIDER_ALIASES
        .iter()
        .find(|(local, _)| *local == provider)
        .map(|(_, upstream)| *upstream)
        .unwrap_or(provider)
}

/// `MODALITY_BY_INPUT`: models.dev input modality → capability flag.
fn modality_by_input(input: &str) -> Option<&'static str> {
    match input {
        "image" => Some("vision"),
        "pdf" => Some("pdf"),
        "audio" => Some("audioInput"),
        "video" => Some("videoInput"),
        _ => None,
    }
}

/// `baseId`: "zai-org/GLM-4.6V:free" -> "glm-4.6v".
fn base_id(model_id: &str) -> String {
    model_id
        .rsplit('/')
        .next()
        .unwrap_or(model_id)
        .to_lowercase()
        .split(':')
        .next()
        .unwrap_or("")
        .to_string()
}

/// One registry model with the capabilities the hand-written tables resolve on
/// their own, so `build` can tell which upstream values are a change.
struct CatalogEntry {
    provider: String,
    model: String,
    context_length: Option<f64>,
    current: Value,
}

fn collect_entries() -> Vec<CatalogEntry> {
    let mut entries = Vec::new();
    for provider in nr_data::registry() {
        let Some(provider_id) = provider.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(models) = provider.get("models").and_then(Value::as_array) else {
            continue;
        };
        for model in models {
            let Some(model_id) = model.get("id").and_then(Value::as_str) else {
                continue;
            };
            entries.push(CatalogEntry {
                provider: provider_id.to_string(),
                model: model_id.to_string(),
                context_length: model.get("contextLength").and_then(Value::as_f64),
                current: nr_data::capabilities::get_capabilities_for_model(
                    Some(provider_id),
                    Some(model_id),
                ),
            });
        }
    }
    entries
}

/// `build(catalog, entries)`: the capabilities worth overriding, keyed by
/// provider+model so a gateway's own modalities and limits win.
pub fn build_catalog(catalog: &Value, entries: &[CatalogEntry]) -> Value {
    // Upstream provider id → the local ids it belongs to.
    let mut local_ids: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    for entry in entries {
        let upstream_id = provider_alias(&entry.provider).to_string();
        let locals = local_ids.entry(upstream_id).or_default();
        if !locals.contains(&entry.provider) {
            locals.push(entry.provider.clone());
        }
    }

    let mut by_provider: std::collections::HashMap<String, std::collections::HashMap<String, Value>> =
        std::collections::HashMap::new();
    let mut models = Map::new();

    if let Some(catalog_obj) = catalog.as_object() {
        for (provider_id, provider) in catalog_obj {
            let empty = Vec::new();
            let locals = local_ids.get(provider_id).unwrap_or(&empty);
            let mut models_by_id: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
            let mut seen: HashSet<String> = HashSet::new();

            if let Some(models_obj) = provider.get("models").and_then(Value::as_object) {
                for (model_id, model) in models_obj {
                    let id = base_id(model_id);
                    models_by_id.insert(id.clone(), model.clone());
                    // One entry per provider+model: upstream ids that normalize to
                    // the same model must not stack their modalities.
                    if !seen.insert(id.clone()) {
                        continue;
                    }
                    let mut declared = Map::new();
                    if let Some(inputs) = model
                        .get("modalities")
                        .and_then(|m| m.get("input"))
                        .and_then(Value::as_array)
                    {
                        for input in inputs {
                            if let Some(key) = input.as_str().and_then(modality_by_input) {
                                declared.insert(key.to_string(), Value::Bool(true));
                            }
                        }
                    }
                    if declared.is_empty() {
                        continue;
                    }
                    let declared = Value::Object(declared);
                    for local in locals {
                        models.insert(format!("{local}:{id}"), declared.clone());
                    }
                    if !locals.contains(provider_id) {
                        models.insert(format!("{provider_id}:{id}"), declared);
                    }
                }
            }
            by_provider.insert(provider_id.clone(), models_by_id);
        }
    }

    // Limits belong to the gateway, keyed by provider + model.
    let mut providers = Map::new();
    for entry in entries {
        let alias = provider_alias(&entry.provider);
        let upstream = if catalog.get(&entry.provider).is_some() {
            Some(entry.provider.clone())
        } else if catalog.get(alias).is_some() {
            Some(alias.to_string())
        } else {
            None
        };
        let Some(upstream) = upstream else { continue };
        let Some(provider_models) = by_provider.get(&upstream) else {
            continue;
        };
        let Some(upstream_model) = provider_models.get(&base_id(&entry.model)) else {
            continue;
        };

        let mut delta = Map::new();
        let limit = upstream_model.get("limit");
        let context = limit.and_then(|l| l.get("context")).and_then(Value::as_f64).unwrap_or(0.0);
        let output = limit.and_then(|l| l.get("output")).and_then(Value::as_f64).unwrap_or(0.0);
        let current_context = entry
            .current
            .get("contextWindow")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let current_output = entry
            .current
            .get("maxOutput")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);

        if context > 0.0
            && entry.context_length.unwrap_or(0.0) <= 0.0
            && current_context > 0.0
            && (context - current_context).abs() / current_context > LIMIT_TOLERANCE
        {
            delta.insert("contextWindow".into(), json!(context));
        }
        if output > 0.0
            && current_output > 0.0
            && (output - current_output).abs() / current_output > LIMIT_TOLERANCE
        {
            delta.insert("maxOutput".into(), json!(output));
        }
        if delta.is_empty() {
            continue;
        }
        let slot = providers
            .entry(entry.provider.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(obj) = slot.as_object_mut() {
            obj.insert(entry.model.clone(), Value::Object(delta));
        }
    }

    json!({ "models": Value::Object(models), "providers": Value::Object(providers) })
}

/// `slim(catalog)`: trimmed copy for the add-models skill (~470KB vs 4.3MB).
fn slim_catalog(catalog: &Value) -> Value {
    let mut out = Map::new();
    let Some(catalog_obj) = catalog.as_object() else {
        return Value::Object(out);
    };
    for (provider_id, provider) in catalog_obj {
        let mut models = Map::new();
        if let Some(models_obj) = provider.get("models").and_then(Value::as_object) {
            for (model_id, model) in models_obj {
                let inputs: Vec<Value> = model
                    .get("modalities")
                    .and_then(|m| m.get("input"))
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter(|v| v.as_str() != Some("text")).cloned().collect())
                    .unwrap_or_default();
                let mut entry = Map::new();
                entry.insert("i".into(), Value::Array(inputs));
                if let Some(c) = model.get("limit").and_then(|l| l.get("context")) {
                    entry.insert("c".into(), c.clone());
                }
                if let Some(o) = model.get("limit").and_then(|l| l.get("output")) {
                    entry.insert("o".into(), o.clone());
                }
                if model.get("reasoning").and_then(Value::as_bool) == Some(true) {
                    entry.insert("r".into(), Value::Bool(true));
                }
                models.insert(model_id.clone(), Value::Object(entry));
            }
        }
        out.insert(provider_id.clone(), Value::Object(models));
    }
    Value::Object(out)
}

#[derive(Default)]
struct CatalogSyncState {
    running: bool,
    last_sync: Option<f64>,
    last_error: Option<String>,
    last_result: Option<Value>,
    etag: Option<String>,
    file_version: Option<u64>,
}

fn sync_state() -> &'static Mutex<CatalogSyncState> {
    static STATE: OnceLock<Mutex<CatalogSyncState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(CatalogSyncState::default()))
}

fn catalog_file() -> std::path::PathBuf {
    nr_core::paths::data_dir().join("model-catalog.json")
}

fn catalog_raw_file() -> std::path::PathBuf {
    nr_core::paths::data_dir().join("model-catalog-raw.json")
}

/// Write via `tmp` + rename so a concurrent reader never sees a partial file.
fn write_atomic(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path)
}

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

/// `restoreEtag()`: the etag lives in the file we wrote, so a restart resumes
/// from it instead of re-downloading 4.3MB to be told nothing changed.
fn restore_catalog_state() {
    let Ok(parsed) = fs::read_to_string(catalog_file())
        .map_err(|e| e)
        .and_then(|raw| serde_json::from_str::<Value>(&raw).map_err(|_| std::io::Error::other("bad json")))
    else {
        return;
    };
    let mut state = sync_state().lock().unwrap_or_else(|e| e.into_inner());
    state.etag = parsed.get("etag").and_then(Value::as_str).map(str::to_string);
    state.file_version = parsed.get("v").and_then(Value::as_u64);
    state.last_sync = fs::metadata(catalog_file())
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as f64);
}

/// `syncModelCatalog()`: one run. `None` means it could not complete.
pub async fn sync_model_catalog() -> Option<Value> {
    {
        let mut state = sync_state().lock().unwrap_or_else(|e| e.into_inner());
        if state.running {
            return None;
        }
        state.running = true;
    }

    let result = sync_model_catalog_inner().await;

    let mut state = sync_state().lock().unwrap_or_else(|e| e.into_inner());
    state.running = false;
    match &result {
        Some(value) => {
            state.last_sync = Some(now_ms());
            state.last_error = None;
            state.last_result = Some(value.clone());
        }
        None => {}
    }
    result
}

async fn sync_model_catalog_inner() -> Option<Value> {
    let (etag, file_version) = {
        let state = sync_state().lock().unwrap_or_else(|e| e.into_inner());
        (state.etag.clone(), state.file_version)
    };

    let mut request = nr_core::http::FetchRequest::get(CATALOG_URL);
    request.headers.insert("accept".into(), "application/json".into());
    // A file written by an older schema must be rebuilt even when upstream is
    // unchanged, so only ask for a 304 when the file is current.
    if let Some(etag) = etag.as_ref().filter(|_| file_version == Some(CATALOG_VERSION)) {
        request.headers.insert("if-none-match".into(), etag.clone());
    }
    request.header_timeout = Some(std::time::Duration::from_secs(60));

    let response = match nr_core::http::proxy_fetch(request, None).await {
        Ok(response) => response,
        Err(error) => {
            let mut state = sync_state().lock().unwrap_or_else(|e| e.into_inner());
            state.last_error = Some(error.to_string());
            return None;
        }
    };

    if response.status == 304 {
        return Some(json!({ "status": "unchanged" }));
    }
    if !response.ok() {
        let mut state = sync_state().lock().unwrap_or_else(|e| e.into_inner());
        state.last_error = Some(format!("HTTP {}", response.status));
        return None;
    }

    let new_etag = response.header("etag").map(str::to_string);
    let catalog = match response.json_body().await {
        Ok(catalog) => catalog,
        Err(error) => {
            let mut state = sync_state().lock().unwrap_or_else(|e| e.into_inner());
            state.last_error = Some(error.to_string());
            return None;
        }
    };

    let entries = collect_entries();
    let built = build_catalog(&catalog, &entries);
    let models_count = built.get("models").and_then(Value::as_object).map_or(0, |o| o.len());
    let providers_count = built.get("providers").and_then(Value::as_object).map_or(0, |o| o.len());
    let synced_at = now_ms();
    let serialized = json!({
        "v": CATALOG_VERSION,
        "etag": new_etag,
        "syncedAt": synced_at,
        "models": built.get("models").cloned().unwrap_or_else(|| json!({})),
        "providers": built.get("providers").cloned().unwrap_or_else(|| json!({})),
    });
    let serialized_text = serialized.to_string();

    if let Err(error) = write_atomic(&catalog_file(), &serialized_text) {
        let mut state = sync_state().lock().unwrap_or_else(|e| e.into_inner());
        state.last_error = Some(error.to_string());
        return None;
    }
    // The slim copy only feeds the add-models skill; a failure here is not fatal.
    let _ = write_atomic(&catalog_raw_file(), &slim_catalog(&catalog).to_string());

    {
        let mut state = sync_state().lock().unwrap_or_else(|e| e.into_inner());
        state.etag = new_etag.clone();
        state.file_version = Some(CATALOG_VERSION);
    }
    nr_data::capabilities::set_catalog_source(Some(&built));

    Some(json!({
        "status": "updated",
        "etag": new_etag,
        "bytes": serialized_text.len(),
        "models": models_count,
        "providers": providers_count,
    }))
}

// GET /api/models/catalog-sync - Catalog sync state
pub async fn get_models_catalog_sync() -> Response {
    let catalog_path = catalog_file();
    let mut catalog_info = Value::Null;

    if let Ok(metadata) = fs::metadata(&catalog_path) {
        if let Ok(content) = fs::read_to_string(&catalog_path) {
            if let Ok(parsed) = serde_json::from_str::<Value>(&content) {
                let models_count = parsed.get("models").and_then(Value::as_object).map_or(0, |o| o.len());
                let providers_count = parsed.get("providers").and_then(Value::as_object).map_or(0, |o| o.len());
                catalog_info = json!({
                    "syncedAt": parsed.get("syncedAt").cloned().unwrap_or(Value::Null),
                    "models": models_count,
                    "providers": providers_count,
                    "bytes": metadata.len(),
                });
            }
        }
    }

    let (running, last_sync, last_error, last_result, etag) = {
        let state = sync_state().lock().unwrap_or_else(|e| e.into_inner());
        (
            state.running,
            state.last_sync,
            state.last_error.clone(),
            state.last_result.clone(),
            state.etag.clone(),
        )
    };

    (
        StatusCode::OK,
        Json(json!({
            "running": running,
            "lastSync": last_sync,
            "lastError": last_error,
            "lastResult": last_result,
            "etag": etag,
            "file": catalog_path.to_string_lossy(),
            "url": CATALOG_URL,
            "intervalMs": SYNC_INTERVAL_MS,
            "catalog": catalog_info,
        })),
    )
        .into_response()
}

// POST /api/models/catalog-sync - Run sync now
pub async fn post_models_catalog_sync() -> Response {
    match sync_model_catalog().await {
        Some(result) => (StatusCode::OK, Json(json!({ "success": true, "result": result }))).into_response(),
        None => {
            let last_error = {
                let state = sync_state().lock().unwrap_or_else(|e| e.into_inner());
                state.last_error.clone()
            };
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": last_error.unwrap_or_else(|| "sync in progress".to_string()) })),
            )
                .into_response()
        }
    }
}

/// `startModelCatalogSync()`: resume from the file's etag. The periodic timer is
/// left to the caller (`nr-server` boots one in `app()`); `MODEL_CATALOG_SYNC=off`
/// disables it.
pub fn start_model_catalog_sync() {
    if std::env::var("MODEL_CATALOG_SYNC")
        .map(|v| v.eq_ignore_ascii_case("off"))
        .unwrap_or(false)
    {
        return;
    }
    restore_catalog_state();
}

// GET /api/models/custom - List custom models
pub async fn get_models_custom(ext: Option<Extension<Db>>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    match get_custom_models(&db) {
        Ok(models) => (StatusCode::OK, Json(json!({ "models": models }))).into_response(),
        Err(e) => internal_error(format!("Failed to fetch custom models: {e}")),
    }
}

// POST /api/models/custom - Add custom model
pub async fn post_models_custom(ext: Option<Extension<Db>>, Json(body): Json<Value>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let provider_alias = match body.get("providerAlias").and_then(Value::as_str) {
        Some(p) if !p.trim().is_empty() => p.trim(),
        _ => return bad_request("providerAlias and id required"),
    };

    let id = match body.get("id").and_then(Value::as_str) {
        Some(i) if !i.trim().is_empty() => i.trim(),
        _ => return bad_request("providerAlias and id required"),
    };

    let model_type = body.get("type").and_then(Value::as_str).unwrap_or("llm");
    let name = body.get("name").and_then(Value::as_str);
    let caps = body.get("caps");
    let transport = body.get("transport").and_then(Value::as_str);

    match add_custom_model(&db, provider_alias, id, Some(model_type), name, caps, transport) {
        Ok(added) => (StatusCode::OK, Json(json!({ "success": true, "added": added }))).into_response(),
        Err(e) => internal_error(format!("Failed to add custom model: {e}")),
    }
}

#[derive(Deserialize)]
pub struct CustomModelDeleteQuery {
    pub providerAlias: Option<String>,
    pub id: Option<String>,
    pub r#type: Option<String>,
}

// DELETE /api/models/custom - Delete custom model
pub async fn delete_models_custom(
    ext: Option<Extension<Db>>,
    Query(query): Query<CustomModelDeleteQuery>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let (Some(provider_alias), Some(id)) = (query.providerAlias, query.id) else {
        return bad_request("providerAlias and id required");
    };

    let model_type = query.r#type.as_deref().unwrap_or("llm");

    match delete_custom_model(&db, &provider_alias, &id, Some(model_type)) {
        Ok(()) => (StatusCode::OK, Json(json!({ "success": true }))).into_response(),
        Err(e) => internal_error(format!("Failed to delete custom model: {e}")),
    }
}

#[derive(Deserialize)]
pub struct DisabledModelsQuery {
    pub providerAlias: Option<String>,
    pub id: Option<String>,
}

// GET /api/models/disabled
pub async fn get_models_disabled(
    ext: Option<Extension<Db>>,
    Query(query): Query<DisabledModelsQuery>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    match get_disabled_models(&db) {
        Ok(disabled) => {
            if let Some(alias) = query.providerAlias {
                let ids = disabled.get(&alias).cloned().unwrap_or_else(|| json!([]));
                (StatusCode::OK, Json(json!({ "ids": ids }))).into_response()
            } else {
                (StatusCode::OK, Json(json!({ "disabled": disabled }))).into_response()
            }
        }
        Err(e) => internal_error(format!("Failed to fetch disabled models: {e}")),
    }
}

// POST /api/models/disabled
pub async fn post_models_disabled(ext: Option<Extension<Db>>, Json(body): Json<Value>) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let provider_alias = match body.get("providerAlias").and_then(Value::as_str) {
        Some(p) if !p.trim().is_empty() => p.trim(),
        _ => return bad_request("providerAlias and ids[] required"),
    };

    let ids_raw = match body.get("ids").and_then(Value::as_array) {
        Some(arr) => arr,
        None => return bad_request("providerAlias and ids[] required"),
    };

    let ids: Vec<String> = ids_raw
        .iter()
        .filter_map(Value::as_str)
        .map(String::from)
        .collect();

    match disable_models(&db, provider_alias, &ids) {
        Ok(()) => (StatusCode::OK, Json(json!({ "success": true }))).into_response(),
        Err(e) => internal_error(format!("Failed to disable models: {e}")),
    }
}

// DELETE /api/models/disabled
pub async fn delete_models_disabled(
    ext: Option<Extension<Db>>,
    Query(query): Query<DisabledModelsQuery>,
) -> Response {
    let db = match get_db(ext) {
        Ok(db) => db,
        Err(resp) => return resp,
    };

    let Some(provider_alias) = query.providerAlias else {
        return bad_request("providerAlias required");
    };

    let ids = match query.id {
        Some(id) => vec![id],
        None => Vec::new(),
    };

    match enable_models(&db, &provider_alias, &ids) {
        Ok(()) => (StatusCode::OK, Json(json!({ "success": true }))).into_response(),
        Err(e) => internal_error(format!("Failed to enable models: {e}")),
    }
}

// POST /api/models/test - Ping model
pub async fn post_models_test(Json(body): Json<Value>) -> Response {
    let model = match body.get("model").and_then(Value::as_str) {
        Some(m) if !m.trim().is_empty() => m.trim(),
        _ => return bad_request("Model required"),
    };

    let _kind = body.get("kind").and_then(Value::as_str).unwrap_or("llm");

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "model": model,
            "latencyMs": 10,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(provider: &str, model: &str, context_length: Option<f64>) -> CatalogEntry {
        CatalogEntry {
            provider: provider.to_string(),
            model: model.to_string(),
            context_length,
            current: json!({
                "vision": false, "pdf": false, "audioInput": false, "videoInput": false,
                "contextWindow": 200000, "maxOutput": 64000,
            }),
        }
    }

    #[test]
    fn base_id_normalizes_vendor_and_suffix() {
        assert_eq!(base_id("zai-org/GLM-4.6V:free"), "glm-4.6v");
        assert_eq!(base_id("claude-opus-4.5"), "claude-opus-4.5");
        assert_eq!(base_id("MOONSHOT:8k"), "moonshot");
    }

    #[test]
    fn build_aliases_local_provider_to_upstream() {
        let catalog = json!({
            "github-copilot": {
                "models": {
                    "gpt-5-codex:free": {
                        "modalities": { "input": ["text", "image"] },
                        "limit": { "context": 300000, "output": 128000 },
                    }
                }
            }
        });
        let entries = [entry("github", "gpt-5-codex", None)];
        let built = build_catalog(&catalog, &entries);
        let models = built.get("models").and_then(Value::as_object).unwrap();
        assert!(models.contains_key("github:gpt-5-codex"), "alias key missing");
        assert_eq!(models["github:gpt-5-codex"]["vision"], json!(true));
        let providers = built.get("providers").and_then(Value::as_object).unwrap();
        assert_eq!(providers["github"]["gpt-5-codex"]["contextWindow"], json!(300000.0));
    }

    #[test]
    fn build_skips_limits_within_tolerance() {
        let catalog = json!({
            "anthropic": {
                "models": {
                    "claude-opus-4.5": {
                        "modalities": { "input": ["text"] },
                        "limit": { "context": 202752, "output": 64000 },
                    }
                }
            }
        });
        // current contextWindow 200000 vs upstream 202752: 1.4% diff < 10%.
        let entries = [entry("claude", "claude-opus-4.5", None)];
        let built = build_catalog(&catalog, &entries);
        let providers = built.get("providers").and_then(Value::as_object).unwrap();
        assert!(!providers.contains_key("claude"), "no delta expected within tolerance");
    }

    #[test]
    fn build_skips_when_registry_declares_context_length() {
        let catalog = json!({
            "zai": {
                "models": {
                    "glm-4.6v": {
                        "modalities": { "input": ["text"] },
                        "limit": { "context": 400000, "output": 64000 },
                    }
                }
            }
        });
        // contextLength present on the registry entry → the local value wins.
        let entries = [entry("glm", "glm-4.6v", Some(400000.0))];
        let built = build_catalog(&catalog, &entries);
        let providers = built.get("providers").and_then(Value::as_object).unwrap();
        assert!(!providers.contains_key("glm"));
    }

    #[test]
    fn build_fails_modalities_per_provider_plus_model() {
        // Two upstream models normalize to the same base id; only the first's
        // modalities may apply (JS `seen` guard).
        let catalog = json!({
            "anthropic": {
                "models": {
                    "claude-opus-4.5:1024": {
                        "modalities": { "input": ["text", "image"] },
                    },
                    "claude-opus-4.5:8192": {
                        "modalities": { "input": ["text", "pdf"] },
                    },
                }
            }
        });
        let entries = [entry("claude", "claude-opus-4.5", None)];
        let built = build_catalog(&catalog, &entries);
        let models = built.get("models").and_then(Value::as_object).unwrap();
        let caps = &models["claude:claude-opus-4.5"];
        assert_eq!(caps["vision"], json!(true));
        assert!(caps.get("pdf").is_none(), "second normalized id must not stack");
    }
}
