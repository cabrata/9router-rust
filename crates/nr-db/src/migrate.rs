//! Port of src/lib/db/migrate.js: versioned migration chain, additive schema
//! sync, one-time legacy JSON import and the pre-schema safety backup.
//!
//! The same `_meta` keys are written as the JS build (`schemaVersion`,
//! `backupSchemaVersion`, `appVersion`, `migratedAt`, `totalRequestsLifetime`),
//! so a database bootstrapped by either implementation keeps migrating with the
//! other.

use crate::backup::{backup_db_lite, backup_file, make_backup_dir, prune_old_backups};
use crate::connection::Db;
use crate::helpers::stringify_json;
use crate::schema::{build_create_table_sql, tables, SCHEMA_VERSION};
use nr_core::Result;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Marker file: prevents re-importing legacy JSON when the user wipes data.sqlite.
const MIGRATED_MARKER: &str = ".migrated-from-json";

/// Legacy JSON files, relative to the data dir (JS `LEGACY_FILES`).
const LEGACY_MAIN: &str = "db.json";
const LEGACY_USAGE: &str = "usage.json";
const LEGACY_DISABLED: &str = "disabledModels.json";
const LEGACY_DETAILS: &str = "request-details.json";

/// `MIGRATIONS` from src/lib/db/migrations/index.js. Each entry bumps
/// `_meta.schemaVersion`; version 1 is the initial bootstrap.
const MIGRATIONS: &[(i32, &str)] = &[(1, "initial")];

/// `latestVersion()`: the highest registered migration version.
pub fn latest_version() -> i32 {
    MIGRATIONS.iter().map(|(v, _)| *v).max().unwrap_or(0)
}

/// Thrown when a row-count assertion fails. The outer transaction rolls back,
/// the legacy JSON is kept intact and the marker is not written, so the next
/// boot retries.
#[derive(Debug)]
pub struct MigrationAborted {
    pub message: String,
    pub dropped_rows: Vec<Value>,
}

impl std::fmt::Display for MigrationAborted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for MigrationAborted {}

// ─── Meta helpers ────────────────────────────────────────────────────────

fn get_meta(db: &Db, key: &str) -> Option<String> {
    db.query_row("SELECT value FROM _meta WHERE key = ?", &[&key], |row| {
        row.get::<_, String>(0)
    })
    .ok()
    .flatten()
}

fn set_meta(db: &Db, key: &str, value: &str) -> Result<()> {
    db.execute_with_params(
        "INSERT INTO _meta(key, value) VALUES(?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [key, value],
    )?;
    Ok(())
}

/// Get the current schema version from _meta table
fn get_schema_version(db: &Db) -> Result<i32> {
    Ok(get_meta(db, "schemaVersion")
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(0))
}

/// Ensure _meta table exists and bootstrap it
fn bootstrap_meta(db: &Db) -> Result<()> {
    let table_defs = tables();
    if let Some(meta_def) = table_defs.get("_meta") {
        let sql = build_create_table_sql("_meta", meta_def);
        db.execute(&sql)?;
    }
    Ok(())
}

/// Check if database is fresh (no tables or only _meta with no rows)
fn is_fresh_db(db: &Db) -> bool {
    match db.query_row("SELECT COUNT(*) as c FROM _meta", &[], |row| row.get::<_, i64>(0)) {
        Ok(Some(count)) => count == 0,
        _ => true, // Table doesn't exist
    }
}

// ─── Versioned migrations runner (skip-version safe) ─────────────────────

fn run_versioned_migrations(db: &Db) -> Result<(usize, i32, i32)> {
    // Bootstrap _meta first so we can read schemaVersion
    bootstrap_meta(db)?;

    let current = get_schema_version(db)?;
    let target = latest_version();
    if current >= target {
        return Ok((0, current, current));
    }

    let mut applied = 0;
    let mut last_applied = current;
    for (version, name) in MIGRATIONS.iter().filter(|(v, _)| *v > current) {
        db.with_tx(|tx| {
            // 001-initial: create every table + index (idempotent).
            if *version == 1 {
                for (table_name, def) in tables().iter() {
                    tx.execute(&build_create_table_sql(table_name, &def))?;
                    for idx in &def.indexes {
                        tx.execute(idx)?;
                    }
                }
            }
            tx.execute_with_params(
                "INSERT INTO _meta(key, value) VALUES('schemaVersion', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [version.to_string().as_str()],
            )?;
            Ok(())
        })?;
        applied += 1;
        last_applied = *version;
        tracing::info!("[DB][migrate] applied #{} {}", version, name);
    }
    Ok((applied, current, last_applied))
}

// ─── Auto-sync (additive only): add missing tables/columns/indexes ───────

fn sync_schema_from_tables(db: &Db) -> Result<()> {
    for (table_name, def) in tables().iter() {
        // Create table if absent
        db.execute(&build_create_table_sql(table_name, def))?;

        // Diff columns
        let existing_cols = db.query_all(&format!("PRAGMA table_info({})", table_name), &[], |row| {
            Ok(row.get::<_, String>(1)?) // column name is at index 1
        })?;
        let existing_set: HashSet<String> = existing_cols.into_iter().collect();

        for (col_name, col_def) in def.columns.iter() {
            if existing_set.contains(col_name) {
                continue;
            }
            // SQLite ADD COLUMN restrictions: strip PRIMARY KEY / UNIQUE, which
            // are only valid at create time (JS: same replacement).
            let safe_def = strip_column_constraints(col_def);
            let alter_sql = format!("ALTER TABLE {} ADD COLUMN {} {}", table_name, col_name, safe_def);
            match db.execute(&alter_sql) {
                Ok(_) => tracing::info!("[DB][sync] +column {}.{}", table_name, col_name),
                Err(e) => tracing::warn!("[DB][sync] add column {}.{} failed: {}", table_name, col_name, e),
            }
        }

        // Indexes (idempotent)
        for idx_sql in &def.indexes {
            let _ = db.execute(idx_sql);
        }
    }
    Ok(())
}

/// `colDef.replace(/PRIMARY KEY( AUTOINCREMENT)?/i, "").replace(/UNIQUE/i, "")`.
fn strip_column_constraints(col_def: &str) -> String {
    let mut out = col_def.to_string();
    for token in ["PRIMARY KEY AUTOINCREMENT", "PRIMARY KEY", "UNIQUE"] {
        // Case-insensitive search, JS `i` flag.
        let mut search_from = 0;
        loop {
            let lower = out.to_lowercase();
            let Some(pos) = lower[search_from..].find(&token.to_lowercase()) else {
                break;
            };
            let start = search_from + pos;
            out.replace_range(start..start + token.len(), "");
            search_from = start;
        }
    }
    out.trim().to_string()
}

// ─── Legacy JSON import (one-time) ───────────────────────────────────────

fn read_json_safe(file: &Path) -> Option<Value> {
    if !file.exists() {
        return None;
    }
    let text = std::fs::read_to_string(file).ok()?;
    serde_json::from_str(&text).ok()
}

/// Insert rows one-by-one, collect failures, then assert COUNT(*) matches the
/// input length (JS `importWithAssertion`).
fn import_with_assertion<F>(
    db: &Db,
    table: &str,
    rows: &[Value],
    mut insert: F,
    row_meta: impl Fn(&Value) -> Value,
) -> Result<()>
where
    F: FnMut(&Value) -> Result<usize>,
{
    let mut dropped = Vec::new();
    for row in rows {
        if let Err(err) = insert(row) {
            let mut meta = row_meta(row);
            if let Some(obj) = meta.as_object_mut() {
                obj.insert("reason".into(), Value::String(err.to_string()));
            }
            dropped.push(meta);
        }
    }
    let inserted: i64 = db
        .query_row(&format!("SELECT COUNT(*) as c FROM {}", table), &[], |row| row.get(0))?
        .unwrap_or(0);
    if inserted != rows.len() as i64 {
        let message = format!(
            "{} row-count mismatch: expected {}, got {}",
            table,
            rows.len(),
            inserted
        );
        let dropped_json = serde_json::Value::Array(dropped.clone());
        tracing::warn!(
            "[DB][migrate] {} row-count mismatch: expected {}, got {}. Dropped: {}",
            table,
            rows.len(),
            inserted,
            dropped_json
        );
        return Err(nr_core::Error::other(
            MigrationAborted { message, dropped_rows: dropped }.to_string(),
        ));
    }
    Ok(())
}

fn str_or_null(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn import_legacy_main(db: &Db, data: &Value) -> Result<()> {
    if !data.is_object() {
        return Ok(());
    }

    if let Some(settings) = data.get("settings") {
        db.execute_with_params(
            "INSERT INTO settings(id, data) VALUES(1, ?) ON CONFLICT(id) DO UPDATE SET data = excluded.data",
            [stringify_json(settings)],
        )?;
    }

    let conns = data.get("providerConnections").and_then(Value::as_array).cloned().unwrap_or_default();
    import_with_assertion(
        db,
        "providerConnections",
        &conns,
        |c| {
            let id = str_or_null(c, "id").unwrap_or_default();
            let provider = str_or_null(c, "provider").unwrap_or_default();
            let auth_type = str_or_null(c, "authType").unwrap_or_else(|| "oauth".into());
            let name = str_or_null(c, "name");
            let email = str_or_null(c, "email");
            let priority = c.get("priority").and_then(Value::as_i64);
            let is_active = if c.get("isActive").and_then(Value::as_bool) == Some(false) { 0 } else { 1 };
            let created = str_or_null(c, "createdAt").unwrap_or_else(now_iso);
            let updated = str_or_null(c, "updatedAt").unwrap_or_else(now_iso);
            let mut rest = c.clone();
            if let Some(obj) = rest.as_object_mut() {
                for f in ["id", "provider", "authType", "name", "email", "priority", "isActive", "createdAt", "updatedAt"] {
                    obj.remove(f);
                }
            }
            db.execute_with_params(
                "INSERT OR REPLACE INTO providerConnections(id, provider, authType, name, email, priority, isActive, data, createdAt, updatedAt) VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![id, provider, auth_type, name, email, priority, is_active, stringify_json(&rest), created, updated],
            )
        },
        |c| json!({ "id": c.get("id").cloned().unwrap_or(Value::Null), "provider": c.get("provider").cloned().unwrap_or(Value::Null), "name": c.get("name").cloned().unwrap_or(Value::Null) }),
    )?;

    let nodes = data.get("providerNodes").and_then(Value::as_array).cloned().unwrap_or_default();
    import_with_assertion(
        db,
        "providerNodes",
        &nodes,
        |n| {
            let id = str_or_null(n, "id").unwrap_or_default();
            let node_type = str_or_null(n, "type");
            let name = str_or_null(n, "name");
            let created = str_or_null(n, "createdAt").unwrap_or_else(now_iso);
            let updated = str_or_null(n, "updatedAt").unwrap_or_else(now_iso);
            let mut rest = n.clone();
            if let Some(obj) = rest.as_object_mut() {
                for f in ["id", "type", "name", "createdAt", "updatedAt"] {
                    obj.remove(f);
                }
            }
            db.execute_with_params(
                "INSERT OR REPLACE INTO providerNodes(id, type, name, data, createdAt, updatedAt) VALUES(?, ?, ?, ?, ?, ?)",
                rusqlite::params![id, node_type, name, stringify_json(&rest), created, updated],
            )
        },
        |n| json!({ "id": n.get("id").cloned().unwrap_or(Value::Null), "type": n.get("type").cloned().unwrap_or(Value::Null), "name": n.get("name").cloned().unwrap_or(Value::Null) }),
    )?;

    let pools = data.get("proxyPools").and_then(Value::as_array).cloned().unwrap_or_default();
    import_with_assertion(
        db,
        "proxyPools",
        &pools,
        |p| {
            let id = str_or_null(p, "id").unwrap_or_default();
            let is_active = if p.get("isActive").and_then(Value::as_bool) == Some(false) { 0 } else { 1 };
            let test_status = str_or_null(p, "testStatus").unwrap_or_else(|| "unknown".into());
            let created = str_or_null(p, "createdAt").unwrap_or_else(now_iso);
            let updated = str_or_null(p, "updatedAt").unwrap_or_else(now_iso);
            let mut rest = p.clone();
            if let Some(obj) = rest.as_object_mut() {
                for f in ["id", "isActive", "testStatus", "createdAt", "updatedAt"] {
                    obj.remove(f);
                }
            }
            db.execute_with_params(
                "INSERT OR REPLACE INTO proxyPools(id, isActive, testStatus, data, createdAt, updatedAt) VALUES(?, ?, ?, ?, ?, ?)",
                rusqlite::params![id, is_active, test_status, stringify_json(&rest), created, updated],
            )
        },
        |p| json!({ "id": p.get("id").cloned().unwrap_or(Value::Null) }),
    )?;

    let keys = data.get("apiKeys").and_then(Value::as_array).cloned().unwrap_or_default();
    import_with_assertion(
        db,
        "apiKeys",
        &keys,
        |k| {
            let id = str_or_null(k, "id").unwrap_or_default();
            let key = str_or_null(k, "key").unwrap_or_default();
            let name = str_or_null(k, "name");
            let machine_id = str_or_null(k, "machineId");
            let is_active = if k.get("isActive").and_then(Value::as_bool) == Some(false) { 0 } else { 1 };
            let created = str_or_null(k, "createdAt").unwrap_or_else(now_iso);
            db.execute_with_params(
                "INSERT OR REPLACE INTO apiKeys(id, key, name, machineId, isActive, createdAt) VALUES(?, ?, ?, ?, ?, ?)",
                rusqlite::params![id, key, name, machine_id, is_active, created],
            )
        },
        |k| json!({ "id": k.get("id").cloned().unwrap_or(Value::Null), "name": k.get("name").cloned().unwrap_or(Value::Null) }),
    )?;

    let combos = data.get("combos").and_then(Value::as_array).cloned().unwrap_or_default();
    import_with_assertion(
        db,
        "combos",
        &combos,
        |c| {
            let id = str_or_null(c, "id").unwrap_or_default();
            let name = str_or_null(c, "name").unwrap_or_default();
            let kind = str_or_null(c, "kind");
            let models = c.get("models").cloned().unwrap_or_else(|| json!([]));
            let created = str_or_null(c, "createdAt").unwrap_or_else(now_iso);
            let updated = str_or_null(c, "updatedAt").unwrap_or_else(now_iso);
            db.execute_with_params(
                "INSERT OR REPLACE INTO combos(id, name, kind, models, createdAt, updatedAt) VALUES(?, ?, ?, ?, ?, ?)",
                rusqlite::params![id, name, kind, stringify_json(&models), created, updated],
            )
        },
        |c| json!({ "id": c.get("id").cloned().unwrap_or(Value::Null), "name": c.get("name").cloned().unwrap_or(Value::Null) }),
    )?;

    if let Some(aliases) = data.get("modelAliases").and_then(Value::as_object) {
        for (alias, model) in aliases {
            db.execute_with_params(
                "INSERT OR REPLACE INTO kv(scope, key, value) VALUES('modelAliases', ?, ?)",
                rusqlite::params![alias, stringify_json(model)],
            )?;
        }
    }
    for m in data.get("customModels").and_then(Value::as_array).cloned().unwrap_or_default() {
        let provider = str_or_null(&m, "providerAlias").unwrap_or_default();
        let id = str_or_null(&m, "id").unwrap_or_default();
        let kind = str_or_null(&m, "type").unwrap_or_else(|| "llm".into());
        db.execute_with_params(
            "INSERT OR REPLACE INTO kv(scope, key, value) VALUES('customModels', ?, ?)",
            rusqlite::params![format!("{}|{}|{}", provider, id, kind), stringify_json(&m)],
        )?;
    }
    if let Some(aliases) = data.get("mitmAlias").and_then(Value::as_object) {
        for (tool, mappings) in aliases {
            db.execute_with_params(
                "INSERT OR REPLACE INTO kv(scope, key, value) VALUES('mitmAlias', ?, ?)",
                rusqlite::params![tool, stringify_json(mappings)],
            )?;
        }
    }
    if let Some(pricing) = data.get("pricing").and_then(Value::as_object) {
        for (provider, models) in pricing {
            db.execute_with_params(
                "INSERT OR REPLACE INTO kv(scope, key, value) VALUES('pricing', ?, ?)",
                rusqlite::params![provider, stringify_json(models)],
            )?;
        }
    }

    Ok(())
}

fn import_legacy_usage(db: &Db, data: &Value) -> Result<()> {
    if !data.is_object() {
        return Ok(());
    }

    for e in data.get("history").and_then(Value::as_array).cloned().unwrap_or_default() {
        let tokens = e.get("tokens").cloned().unwrap_or_else(|| json!({}));
        let pick = |keys: &[&str]| -> i64 {
            keys.iter()
                .find_map(|k| tokens.get(*k).and_then(Value::as_i64))
                .unwrap_or(0)
        };
        let timestamp = str_or_null(&e, "timestamp").unwrap_or_else(now_iso);
        db.execute_with_params(
            "INSERT INTO usageHistory(timestamp, provider, model, connectionId, apiKey, endpoint, promptTokens, completionTokens, cost, status, tokens, meta) VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                timestamp,
                str_or_null(&e, "provider"),
                str_or_null(&e, "model"),
                str_or_null(&e, "connectionId"),
                str_or_null(&e, "apiKey"),
                str_or_null(&e, "endpoint"),
                pick(&["prompt_tokens", "input_tokens"]),
                pick(&["completion_tokens", "output_tokens"]),
                e.get("cost").and_then(Value::as_f64).unwrap_or(0.0),
                str_or_null(&e, "status").unwrap_or_else(|| "ok".into()),
                stringify_json(&tokens),
                stringify_json(&json!({})),
            ],
        )?;
    }

    if let Some(days) = data.get("dailySummary").and_then(Value::as_object) {
        for (date_key, day) in days {
            db.execute_with_params(
                "INSERT OR REPLACE INTO usageDaily(dateKey, data) VALUES(?, ?)",
                rusqlite::params![date_key, stringify_json(day)],
            )?;
        }
    }

    if let Some(total) = data.get("totalRequestsLifetime").and_then(Value::as_i64) {
        set_meta(db, "totalRequestsLifetime", &total.to_string())?;
    }

    Ok(())
}

fn import_legacy_disabled(db: &Db, data: &Value) -> Result<()> {
    let Some(disabled) = data.get("disabled").and_then(Value::as_object) else {
        return Ok(());
    };
    for (provider, ids) in disabled {
        let value = if ids.is_array() { ids.clone() } else { json!([]) };
        db.execute_with_params(
            "INSERT OR REPLACE INTO kv(scope, key, value) VALUES('disabledModels', ?, ?)",
            rusqlite::params![provider, stringify_json(&value)],
        )?;
    }
    Ok(())
}

fn import_legacy_details(db: &Db, data: &Value) -> Result<()> {
    let Some(records) = data.get("records").and_then(Value::as_array) else {
        return Ok(());
    };
    for r in records {
        db.execute_with_params(
            "INSERT OR REPLACE INTO requestDetails(id, timestamp, provider, model, connectionId, status, data) VALUES(?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                str_or_null(r, "id").unwrap_or_default(),
                str_or_null(r, "timestamp").unwrap_or_else(now_iso),
                str_or_null(r, "provider"),
                str_or_null(r, "model"),
                str_or_null(r, "connectionId"),
                str_or_null(r, "status"),
                stringify_json(r),
            ],
        )?;
    }
    Ok(())
}

// ─── Main entry ──────────────────────────────────────────────────────────

/// Run the migration chain once per database lifetime.
///
/// `data_dir` is `$DATA_DIR` — where the legacy JSON files and the backup
/// directory live. [`Db::open`](crate::connection::Db::open) derives it from the
/// database path, so the public behaviour matches the JS boot sequence.
pub fn run_migration_once(db: &Db) -> Result<()> {
    let data_dir = data_dir_of(db);
    run_migration(db, &data_dir)
}

/// Data dir for a database: `<DATA_DIR>/db/data.sqlite` → `<DATA_DIR>`.
fn data_dir_of(db: &Db) -> PathBuf {
    db.path()
        .parent() // …/db
        .and_then(Path::parent) // …/<DATA_DIR>
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub(crate) fn run_migration(db: &Db, data_dir: &Path) -> Result<()> {
    // Capture freshness BEFORE migrations stamp _meta (otherwise a brand-new DB
    // is misclassified as non-fresh once schemaVersion is written).
    let fresh = is_fresh_db(db);

    // Prune stale backups every boot so old oversized backups shrink to KEEP.
    prune_old_backups(data_dir);

    // Bootstrap _meta so we can read the stored backup schema version below.
    bootstrap_meta(db)?;

    // Detect a pending schema change via the central SCHEMA_VERSION const; take
    // a lightweight backup BEFORE any schema mutation below.
    let stored_schema_ver = get_meta(db, "backupSchemaVersion")
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(0);
    if !fresh && stored_schema_ver < SCHEMA_VERSION {
        let backup = (|| -> Result<PathBuf> {
            let dir = make_backup_dir(
                data_dir,
                &format!("schema-{}-to-{}", stored_schema_ver, SCHEMA_VERSION),
            )?;
            backup_db_lite(db, &dir, "data.sqlite")
        })();
        match backup {
            Ok(dir) => tracing::info!(
                "[DB][migrate] pre-schema backup {} → {}: {}",
                stored_schema_ver,
                SCHEMA_VERSION,
                dir.display()
            ),
            Err(e) => tracing::warn!("[DB][migrate] pre-schema backup failed (continuing): {e}"),
        }
        prune_old_backups(data_dir);
    }

    // 1. Always run the versioned migration chain (skip-version safe)
    run_versioned_migrations(db)?;

    // 2. Additive sync (auto add missing columns/indexes declared in TABLES)
    sync_schema_from_tables(db)?;

    // Stamp the schema version just reached so future boots skip the backup.
    set_meta(db, "backupSchemaVersion", &SCHEMA_VERSION.to_string())?;

    // 3. One-time legacy JSON import (only if the DB was fresh on entry)
    let already_imported = data_dir.join(MIGRATED_MARKER).exists();
    let legacy_main = read_json_safe(&data_dir.join(LEGACY_MAIN));
    let legacy_usage = read_json_safe(&data_dir.join(LEGACY_USAGE));
    let legacy_disabled = read_json_safe(&data_dir.join(LEGACY_DISABLED));
    let legacy_details = read_json_safe(&data_dir.join(LEGACY_DETAILS));
    let has_legacy =
        legacy_main.is_some() || legacy_usage.is_some() || legacy_disabled.is_some() || legacy_details.is_some();

    if fresh && has_legacy && !already_imported {
        let backup_dir = make_backup_dir(data_dir, "migrate-from-json")?;
        for file in [LEGACY_MAIN, LEGACY_USAGE, LEGACY_DISABLED, LEGACY_DETAILS] {
            backup_file(&data_dir.join(file), &backup_dir, None);
        }

        // `BEGIN`/`COMMIT` through Db so the import helpers can share the
        // handle (same shape as the JS adapter.transaction()).
        let imported = db.transaction(|db| {
            if let Some(main) = &legacy_main {
                import_legacy_main(db, main)?;
            }
            if let Some(usage) = &legacy_usage {
                import_legacy_usage(db, usage)?;
            }
            if let Some(disabled) = &legacy_disabled {
                import_legacy_disabled(db, disabled)?;
            }
            if let Some(details) = &legacy_details {
                import_legacy_details(db, details)?;
            }
            set_meta(db, "appVersion", env!("CARGO_PKG_VERSION"))?;
            set_meta(db, "backupSchemaVersion", &SCHEMA_VERSION.to_string())?;
            set_meta(db, "migratedAt", &now_iso())?;
            Ok(())
        });

        if let Err(err) = imported {
            // MigrationAborted: legacy JSON kept, marker not written → next boot retries.
            tracing::error!(
                "[DB][migrate] aborted: {} | legacy JSON kept | backup: {}",
                err,
                backup_dir.display()
            );
            return Ok(());
        }

        let _ = std::fs::write(data_dir.join(MIGRATED_MARKER), now_iso());
        prune_old_backups(data_dir);
        tracing::info!(
            "[DB][migrate] JSON → SQLite | legacy JSON kept at {} | backup: {}",
            data_dir.display(),
            backup_dir.display()
        );
        return Ok(());
    }

    // Track the app version for informational purposes only. App version bumps
    // no longer trigger a DB backup — only real schema changes do.
    let new_ver = env!("CARGO_PKG_VERSION");
    if get_meta(db, "appVersion").as_deref() != Some(new_ver) {
        set_meta(db, "appVersion", new_ver)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn migration_runs_once() {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        assert!(db.is_valid());
    }

    #[test]
    fn schema_version_tracking() {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        assert_eq!(get_schema_version(&db).unwrap(), latest_version());
        assert_eq!(get_meta(&db, "backupSchemaVersion").as_deref(), Some("3"));
    }

    #[test]
    fn strip_column_constraints_matches_js() {
        // Expected values produced by the JS implementation
        // (`colDef.replace(/PRIMARY KEY( AUTOINCREMENT)?/i, "").replace(/UNIQUE/i, "").trim()`).
        assert_eq!(strip_column_constraints("TEXT PRIMARY KEY"), "TEXT");
        assert_eq!(strip_column_constraints("INTEGER PRIMARY KEY AUTOINCREMENT"), "INTEGER");
        // Double space is what JS emits; SQLite accepts it in ADD COLUMN.
        assert_eq!(strip_column_constraints("TEXT UNIQUE NOT NULL"), "TEXT  NOT NULL");
        assert_eq!(strip_column_constraints("INTEGER DEFAULT 0"), "INTEGER DEFAULT 0");
    }

    #[test]
    fn legacy_json_is_imported_once() {
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path();
        std::fs::create_dir_all(data_dir.join("db")).unwrap();
        std::fs::write(
            data_dir.join("db.json"),
            serde_json::to_string(&json!({
                "settings": { "requireApiKey": false },
                "providerConnections": [{
                    "id": "c1", "provider": "openai", "authType": "apikey",
                    "name": "Main", "apiKey": "sk-x",
                    "createdAt": "2026-01-01T00:00:00.000Z",
                    "updatedAt": "2026-01-01T00:00:00.000Z"
                }],
                "apiKeys": [{ "id": "k1", "key": "sk-abc", "name": "Key" }],
                "modelAliases": { "fast": "openai/gpt-4o" }
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            data_dir.join("usage.json"),
            serde_json::to_string(&json!({
                "history": [{ "timestamp": "2026-01-02T00:00:00.000Z", "provider": "openai", "model": "gpt-4o", "tokens": { "prompt_tokens": 3, "completion_tokens": 4 } }],
                "totalRequestsLifetime": 7
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            data_dir.join("disabledModels.json"),
            serde_json::to_string(&json!({ "disabled": { "openai": ["gpt-3.5"] } })).unwrap(),
        )
        .unwrap();

        let db = Db::open_in(data_dir).unwrap();

        // Legacy rows landed in the SQLite tables.
        let conns: i64 = db.query_row("SELECT COUNT(*) FROM providerConnections", &[], |r| r.get(0)).unwrap().unwrap();
        assert_eq!(conns, 1);
        let usage: i64 = db.query_row("SELECT COUNT(*) FROM usageHistory", &[], |r| r.get(0)).unwrap().unwrap();
        assert_eq!(usage, 1);
        assert_eq!(get_meta(&db, "totalRequestsLifetime").as_deref(), Some("7"));
        assert!(data_dir.join(MIGRATED_MARKER).exists());

        // A second boot does not re-import (marker + non-fresh DB).
        drop(db);
        let db = Db::open_in(data_dir).unwrap();
        let usage_again: i64 = db.query_row("SELECT COUNT(*) FROM usageHistory", &[], |r| r.get(0)).unwrap().unwrap();
        assert_eq!(usage_again, 1);
    }

    #[test]
    fn failed_import_rolls_back_and_keeps_marker_absent() {
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path();
        std::fs::create_dir_all(data_dir.join("db")).unwrap();
        // Two apiKeys with distinct ids but the same UNIQUE key: INSERT OR
        // REPLACE silently collapses them, so the row-count assertion fails and
        // the whole import rolls back (JS MigrationAborted branch).
        std::fs::write(
            data_dir.join("db.json"),
            serde_json::to_string(&json!({
                "apiKeys": [
                    { "id": "a", "key": "sk-dupe", "name": "one" },
                    { "id": "b", "key": "sk-dupe", "name": "two" }
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        let db = Db::open_in(data_dir).unwrap();
        let keys: i64 = db.query_row("SELECT COUNT(*) FROM apiKeys", &[], |r| r.get(0)).unwrap().unwrap();
        assert_eq!(keys, 0, "import aborted → transaction rolled back");
        assert!(!data_dir.join(MIGRATED_MARKER).exists(), "marker not written");
    }
}
