//! Compatibility tests: the Rust layer must read and write a `data.sqlite`
//! produced by the JS build, and the JS build must be able to keep using the
//! file after Rust wrote to it.
//!
//! The DDL below is copied **verbatim** from `../9router/src/lib/db/schema.js`
//! (`buildCreateTableSql()` output for every entry of `TABLES`) plus the
//! `PRAGMA_SQL` block, so a schema drift in either implementation fails here.
//!
//! Two shapes are covered:
//!  1. a fresh JS boot (tables in declaration order), and
//!  2. an *old* JS boot whose `apiKeys` predates `accessRestricted` /
//!     `accessAllow` and whose `requestLogs` predates some of its columns, i.e.
//!     a file grown by `syncSchemaFromTables()` via ALTER TABLE — that is where
//!     column *order* stops matching the declaration and `SELECT *` breaks.

use nr_db::repos::*;
use nr_db::Db;
use serde_json::{json, Value};
use tempfile::TempDir;

/// `PRAGMA_SQL` from src/lib/db/schema.js.
const JS_PRAGMA_SQL: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA temp_store = MEMORY;
PRAGMA mmap_size = 30000000;
PRAGMA cache_size = -64000;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
";

/// Every `buildCreateTableSql(name, def)` + index from src/lib/db/schema.js,
/// in declaration order.
const JS_SCHEMA_DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS _meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS settings (id INTEGER PRIMARY KEY CHECK (id = 1), data TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS providerConnections (id TEXT PRIMARY KEY, provider TEXT NOT NULL, authType TEXT NOT NULL, name TEXT, email TEXT, priority INTEGER, isActive INTEGER DEFAULT 1, data TEXT NOT NULL, createdAt TEXT NOT NULL, updatedAt TEXT NOT NULL)",
    "CREATE INDEX IF NOT EXISTS idx_pc_provider ON providerConnections(provider)",
    "CREATE INDEX IF NOT EXISTS idx_pc_provider_active ON providerConnections(provider, isActive)",
    "CREATE INDEX IF NOT EXISTS idx_pc_priority ON providerConnections(provider, priority)",
    "CREATE TABLE IF NOT EXISTS providerNodes (id TEXT PRIMARY KEY, type TEXT, name TEXT, data TEXT NOT NULL, createdAt TEXT NOT NULL, updatedAt TEXT NOT NULL)",
    "CREATE INDEX IF NOT EXISTS idx_pn_type ON providerNodes(type)",
    "CREATE TABLE IF NOT EXISTS proxyPools (id TEXT PRIMARY KEY, isActive INTEGER DEFAULT 1, testStatus TEXT, data TEXT NOT NULL, createdAt TEXT NOT NULL, updatedAt TEXT NOT NULL)",
    "CREATE INDEX IF NOT EXISTS idx_pp_active ON proxyPools(isActive)",
    "CREATE INDEX IF NOT EXISTS idx_pp_status ON proxyPools(testStatus)",
    "CREATE TABLE IF NOT EXISTS apiKeys (id TEXT PRIMARY KEY, key TEXT UNIQUE NOT NULL, name TEXT, machineId TEXT, isActive INTEGER DEFAULT 1, expiredAt TEXT, limitConcurrent INTEGER DEFAULT 0, rpm INTEGER DEFAULT 0, createdAt TEXT NOT NULL, allowedModels TEXT, allowedEndpoints TEXT, accessRestricted INTEGER DEFAULT 0, accessAllow TEXT)",
    "CREATE INDEX IF NOT EXISTS idx_ak_key ON apiKeys(key)",
    "CREATE TABLE IF NOT EXISTS combos (id TEXT PRIMARY KEY, name TEXT UNIQUE NOT NULL, kind TEXT, models TEXT NOT NULL, createdAt TEXT NOT NULL, updatedAt TEXT NOT NULL)",
    "CREATE INDEX IF NOT EXISTS idx_combo_name ON combos(name)",
    "CREATE TABLE IF NOT EXISTS kv (scope TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY (scope, key))",
    "CREATE INDEX IF NOT EXISTS idx_kv_scope ON kv(scope)",
    "CREATE TABLE IF NOT EXISTS usageHistory (id INTEGER PRIMARY KEY AUTOINCREMENT, timestamp TEXT NOT NULL, provider TEXT, model TEXT, connectionId TEXT, apiKey TEXT, endpoint TEXT, promptTokens INTEGER DEFAULT 0, completionTokens INTEGER DEFAULT 0, cost REAL DEFAULT 0, status TEXT, tokens TEXT, meta TEXT)",
    "CREATE INDEX IF NOT EXISTS idx_uh_ts ON usageHistory(timestamp DESC)",
    "CREATE INDEX IF NOT EXISTS idx_uh_provider ON usageHistory(provider)",
    "CREATE INDEX IF NOT EXISTS idx_uh_model ON usageHistory(model)",
    "CREATE INDEX IF NOT EXISTS idx_uh_conn ON usageHistory(connectionId)",
    "CREATE TABLE IF NOT EXISTS usageDaily (dateKey TEXT PRIMARY KEY, data TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS requestDetails (id TEXT PRIMARY KEY, timestamp TEXT NOT NULL, provider TEXT, model TEXT, connectionId TEXT, status TEXT, data TEXT NOT NULL)",
    "CREATE INDEX IF NOT EXISTS idx_rd_ts ON requestDetails(timestamp DESC)",
    "CREATE INDEX IF NOT EXISTS idx_rd_provider ON requestDetails(provider)",
    "CREATE INDEX IF NOT EXISTS idx_rd_model ON requestDetails(model)",
    "CREATE INDEX IF NOT EXISTS idx_rd_conn ON requestDetails(connectionId)",
    "CREATE TABLE IF NOT EXISTS requestLogs (id INTEGER PRIMARY KEY AUTOINCREMENT, timestamp TEXT NOT NULL, apiKeyId TEXT, apiKeyName TEXT, apiKeyMasked TEXT, ip TEXT, method TEXT, path TEXT, endpointKind TEXT, model TEXT, provider TEXT, resolvedModel TEXT, status INTEGER, stream INTEGER DEFAULT 0, promptTokens INTEGER DEFAULT 0, completionTokens INTEGER DEFAULT 0, durationMs INTEGER DEFAULT 0, ttftMs INTEGER DEFAULT 0, tps REAL DEFAULT 0, userAgent TEXT, error TEXT)",
    "CREATE INDEX IF NOT EXISTS idx_rl_ts ON requestLogs(timestamp DESC)",
    "CREATE INDEX IF NOT EXISTS idx_rl_key ON requestLogs(apiKeyId)",
];

/// Create `<dir>/db/data.sqlite` the way the JS driver does: PRAGMAs, the DDL
/// above, then `_meta.schemaVersion`/`backupSchemaVersion` stamped by migrate.js.
fn make_js_database(dir: &TempDir, extra_legacy_columns: bool) -> std::path::PathBuf {
    let db_path = dir.path().join("db").join("data.sqlite");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(JS_PRAGMA_SQL).unwrap();
    for stmt in JS_SCHEMA_DDL {
        conn.execute_batch(stmt).unwrap();
    }

    if extra_legacy_columns {
        // Simulate a v1 file: apiKeys without the access columns, requestLogs
        // without the later telemetry columns. syncSchemaFromTables() adds them
        // at the END, which is exactly the ordering hazard.
        conn.execute_batch("DROP TABLE apiKeys").unwrap();
        conn.execute_batch(
            "CREATE TABLE apiKeys (id TEXT PRIMARY KEY, key TEXT UNIQUE NOT NULL, name TEXT, machineId TEXT, isActive INTEGER DEFAULT 1, expiredAt TEXT, limitConcurrent INTEGER DEFAULT 0, rpm INTEGER DEFAULT 0, createdAt TEXT NOT NULL, allowedModels TEXT, allowedEndpoints TEXT)",
        )
        .unwrap();
        conn.execute_batch("DROP TABLE requestLogs").unwrap();
        conn.execute_batch(
            "CREATE TABLE requestLogs (id INTEGER PRIMARY KEY AUTOINCREMENT, timestamp TEXT NOT NULL, apiKeyId TEXT, apiKeyName TEXT, apiKeyMasked TEXT, ip TEXT, method TEXT, path TEXT, endpointKind TEXT, model TEXT, status INTEGER)",
        )
        .unwrap();
    }

    conn.execute_batch(
        "INSERT INTO _meta(key, value) VALUES('schemaVersion', '1')
         ON CONFLICT(key) DO UPDATE SET value = excluded.value;
         INSERT INTO _meta(key, value) VALUES('backupSchemaVersion', '1')
         ON CONFLICT(key) DO UPDATE SET value = excluded.value;
         INSERT INTO _meta(key, value) VALUES('appVersion', '0.5.116')
         ON CONFLICT(key) DO UPDATE SET value = excluded.value;",
    )
    .unwrap();
    drop(conn);
    db_path
}

/// Seed rows exactly as the JS repos write them (SQL lifted from
/// `src/lib/db/repos/*.js`).
fn seed_js_rows(db_path: &std::path::Path, legacy_columns: bool) {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    let api_key_insert = if legacy_columns {
        // v1 file: no access* columns yet.
        "INSERT INTO apiKeys(id, key, name, machineId, isActive, expiredAt, limitConcurrent, rpm, createdAt, allowedModels, allowedEndpoints)
           VALUES('key-1', 'sk-live-abcdefghijkl', 'Main key', 'machine1234', 1, NULL, 2, 5, '2026-01-01T00:00:00.000Z', '[\"openai/gpt-4o\"]', '[\"chat\"]');"
    } else {
        "INSERT INTO apiKeys(id, key, name, machineId, isActive, expiredAt, limitConcurrent, rpm, createdAt, allowedModels, allowedEndpoints, accessRestricted, accessAllow)
           VALUES('key-1', 'sk-live-abcdefghijkl', 'Main key', 'machine1234', 1, NULL, 2, 5, '2026-01-01T00:00:00.000Z', '[\"openai/gpt-4o\"]', '[\"chat\"]', 1, '[\"openai/gpt-4o\"]');"
    };
    let request_log_insert = if legacy_columns {
        "INSERT INTO requestLogs(timestamp, apiKeyId, apiKeyName, apiKeyMasked, ip, method, path, endpointKind, model, status)
           VALUES('2026-01-02T00:00:00.000Z', 'key-1', 'Main key', 'sk-live...ijkl', '127.0.0.1', 'POST', '/v1/chat/completions', 'chat', 'gpt-4o', 200);"
    } else {
        "INSERT INTO requestLogs(timestamp, apiKeyId, apiKeyName, apiKeyMasked, ip, method, path, endpointKind, model, provider, resolvedModel, status, stream, promptTokens, completionTokens, durationMs, ttftMs, tps, userAgent, error)
           VALUES('2026-01-02T00:00:00.000Z', 'key-1', 'Main key', 'sk-live...ijkl', '127.0.0.1', 'POST', '/v1/chat/completions', 'chat', 'gpt-4o', 'openai', 'gpt-4o', 200, 0, 10, 20, 100, 10, 20.5, 'ua', NULL);"
    };

    let head = r#"INSERT INTO settings(id, data) VALUES(1, '{"cloudEnabled":false,"requireApiKey":true,"stickyRoundRobinLimit":3}');
         INSERT INTO providerConnections(id, provider, authType, name, email, priority, isActive, data, createdAt, updatedAt)
           VALUES('conn-1', 'openai', 'apikey', 'Main', NULL, 1, 1, '{"apiKey":"sk-test-123456789","providerSpecificData":{"baseUrl":"https://x.test/v1"}}', '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z');
         INSERT INTO providerNodes(id, type, name, data, createdAt, updatedAt)
           VALUES('node-1', 'openai-compatible', 'Node', '{"baseUrl":"https://n.test/v1","prefix":"nc"}', '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z');
         INSERT INTO proxyPools(id, isActive, testStatus, data, createdAt, updatedAt)
           VALUES('pool-1', 1, 'active', '{"proxyUrl":"http://127.0.0.1:8080"}', '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z');
         "#;

    let tail = r#"INSERT INTO combos(id, name, kind, models, createdAt, updatedAt)
           VALUES('combo-1', 'fast', 'fallback', '["openai/gpt-4o","openai/gpt-4o-mini"]', '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z');
         INSERT INTO kv(scope, key, value) VALUES('modelAliases', 'fast', '"openai/gpt-4o-mini"');
         INSERT INTO kv(scope, key, value) VALUES('customModels', 'openai|custom-1|llm', '{"providerAlias":"openai","id":"custom-1","type":"llm","name":"Custom"}');
         INSERT INTO kv(scope, key, value) VALUES('mitmAlias', 'cursor', '{"gpt-5":"openai/gpt-4o"}');
         INSERT INTO kv(scope, key, value) VALUES('pricing', 'openai', '{"gpt-4o":{"input":2.5,"output":10}}');
         INSERT INTO kv(scope, key, value) VALUES('disabledModels', 'openai', '["gpt-3.5"]');
         INSERT INTO usageHistory(timestamp, provider, model, connectionId, apiKey, endpoint, promptTokens, completionTokens, cost, status, tokens, meta)
           VALUES('2026-01-02T00:00:00.000Z', 'openai', 'gpt-4o', 'conn-1', 'sk-live-abcdefghijkl', '/v1/chat/completions', 10, 20, 0.001, 'ok', '{"prompt_tokens":10,"completion_tokens":20}', '{}');
         INSERT INTO usageDaily(dateKey, data) VALUES('2026-01-02', '{"requests":1,"promptTokens":10,"completionTokens":20,"cost":0.001,"byProvider":{"openai":{"requests":1,"promptTokens":10,"completionTokens":20,"cost":0.001}},"byModel":{},"byAccount":{},"byApiKey":{},"byEndpoint":{}}');
         INSERT INTO requestDetails(id, timestamp, provider, model, connectionId, status, data)
           VALUES('det-1', '2026-01-02T00:00:00.000Z', 'openai', 'gpt-4o', 'conn-1', 'ok', '{"id":"det-1","provider":"openai","model":"gpt-4o"}');
         INSERT INTO _meta(key, value) VALUES('totalRequestsLifetime', '1')
           ON CONFLICT(key) DO UPDATE SET value = excluded.value;"#;

    conn.execute_batch(&format!("{head}{api_key_insert}{request_log_insert}{tail}"))
        .unwrap();
}

fn open_with_js_file(dir: &TempDir, legacy_columns: bool) -> (Db, std::path::PathBuf) {
    let path = make_js_database(dir, legacy_columns);
    seed_js_rows(&path, legacy_columns);
    // Db::open runs the ported migrate chain over the JS file.
    let db = Db::open(&path).unwrap();
    (db, path)
}

#[test]
fn js_created_schema_is_adopted_without_changes() {
    let dir = TempDir::new().unwrap();
    let (db, _path) = open_with_js_file(&dir, false);

    // Every JS table still exists and no column was dropped/retyped.
    let tables: Vec<String> = db
        .query_all(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            &[],
            |row| row.get(0),
        )
        .unwrap();
    for expected in [
        "_meta", "apiKeys", "combos", "kv", "providerConnections", "providerNodes", "proxyPools",
        "requestDetails", "requestLogs", "settings", "usageDaily", "usageHistory",
    ] {
        assert!(tables.iter().any(|t| t == expected), "missing table {expected}");
    }

    // Schema version stamped to the newest migration by the Rust runner.
    let version: String = db
        .query_row("SELECT value FROM _meta WHERE key='schemaVersion'", &[], |r| r.get(0))
        .unwrap()
        .unwrap();
    assert_eq!(version, nr_db::migrate::latest_version().to_string());
    let backup_version: String = db
        .query_row("SELECT value FROM _meta WHERE key='backupSchemaVersion'", &[], |r| r.get(0))
        .unwrap()
        .unwrap();
    assert_eq!(backup_version, "3");
}

#[test]
fn all_repos_read_js_written_rows() {
    let dir = TempDir::new().unwrap();
    let (db, _path) = open_with_js_file(&dir, false);

    // settings
    let settings = get_settings(&db).unwrap();
    assert_eq!(settings["requireApiKey"], json!(true));
    assert_eq!(settings["stickyRoundRobinLimit"], json!(3));

    // connections
    let conns = get_provider_connections(&db, None).unwrap();
    assert_eq!(conns.len(), 1);
    assert_eq!(conns[0]["apiKey"], json!("sk-test-123456789"));

    // nodes / proxy pools
    assert_eq!(get_provider_nodes(&db, None).unwrap().len(), 1);
    assert_eq!(get_proxy_pools(&db, None).unwrap()[0]["testStatus"], json!("active"));

    // api keys: including the access columns, which a JS file may have written
    let keys = get_api_keys(&db).unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].key, "sk-live-abcdefghijkl");
    assert_eq!(keys[0].limit_concurrent, 2);
    assert_eq!(keys[0].rpm, 5);
    assert_eq!(keys[0].allowed_models, vec!["openai/gpt-4o".to_string()]);
    assert_eq!(keys[0].allowed_endpoints, vec!["chat".to_string()]);
    assert!(keys[0].access.restricted);
    assert_eq!(keys[0].access.allow, vec!["openai/gpt-4o".to_string()]);

    let by_key = get_api_key_by_key(&db, "sk-live-abcdefghijkl").unwrap().unwrap();
    assert_eq!(by_key.id, "key-1");
    assert!(validate_api_key(&db, "sk-live-abcdefghijkl").unwrap());

    // combos
    let combos = get_combos(&db).unwrap();
    assert_eq!(combos.len(), 1);
    assert_eq!(combos[0].models, vec!["openai/gpt-4o".to_string(), "openai/gpt-4o-mini".to_string()]);

    // aliases / custom models / mitm / pricing
    assert_eq!(get_model_aliases(&db).unwrap()["fast"], json!("openai/gpt-4o-mini"));
    assert_eq!(get_custom_models(&db).unwrap().len(), 1);
    assert_eq!(get_mitm_alias(&db, Some("cursor")).unwrap()["gpt-5"], json!("openai/gpt-4o"));
    let pricing = get_pricing(&db).unwrap();
    assert!(pricing.is_object());

    // disabled models
    assert_eq!(get_disabled_by_provider(&db, "openai").unwrap(), vec!["gpt-3.5".to_string()]);

    // usage history / daily aggregates / recent logs
    let history = get_usage_history(&db, None).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["provider"], json!("openai"));
    let stats = get_usage_stats(&db, Some("all")).unwrap();
    assert!(stats["totalRequests"].as_i64().unwrap() >= 1);
    let chart = get_chart_data(&db, Some("7d")).unwrap();
    assert!(!chart.is_empty());

    // request details
    let details = get_request_details_page(&db, None).unwrap();
    assert_eq!(details["pagination"]["totalItems"], json!(1));
    assert_eq!(get_distinct_providers(&db).unwrap(), vec!["openai".to_string()]);

    // request logs
    let logs = get_request_logs(&db, &RequestLogFilter::from_json(None)).unwrap();
    assert_eq!(logs["logs"].as_array().unwrap().len(), 1);
    assert_eq!(logs["logs"][0]["stream"], json!(false));
    assert_eq!(logs["pagination"]["totalItems"], json!(1));

    // meta written by the JS boot is preserved
    let lifetime: String = db
        .query_row("SELECT value FROM _meta WHERE key='totalRequestsLifetime'", &[], |r| r.get(0))
        .unwrap()
        .unwrap();
    assert_eq!(lifetime, "1");
}

#[test]
fn old_js_file_with_legacy_column_order_still_reads() {
    let dir = TempDir::new().unwrap();
    let (db, _path) = open_with_js_file(&dir, true);

    // migrate's additive sync must have appended the new columns.
    let api_key_cols: Vec<String> = db
        .query_all("PRAGMA table_info(apiKeys)", &[], |row| row.get::<_, String>(1))
        .unwrap();
    assert!(api_key_cols.contains(&"accessRestricted".to_string()));
    assert!(api_key_cols.contains(&"accessAllow".to_string()));
    // Appended at the end → physical order differs from the declaration order.
    assert_eq!(api_key_cols.last().unwrap(), "accessAllow");

    // apiKeys rows written before the columns existed read as unrestricted.
    db.execute("INSERT INTO apiKeys(id, key, name, machineId, isActive, createdAt, allowedModels, allowedEndpoints) VALUES('legacy-key', 'sk-legacy-0001', 'Legacy', 'm-1', 1, '2025-12-01T00:00:00.000Z', NULL, NULL)")
        .unwrap();
    let legacy = get_api_key_by_key(&db, "sk-legacy-0001").unwrap().unwrap();
    assert_eq!(legacy.id, "legacy-key");
    assert!(legacy.is_active);
    assert!(!legacy.access.restricted);
    assert!(legacy.allowed_models.is_empty());

    // key-1 (seeded by the JS-shaped file) plus the pre-columns row above.
    let mut all: Vec<String> = get_api_keys(&db).unwrap().into_iter().map(|k| k.key).collect();
    all.sort();
    assert_eq!(all, vec!["sk-legacy-0001", "sk-live-abcdefghijkl"]);

    // requestLogs: columns appended after the original set still map by name.
    let log_cols: Vec<String> = db
        .query_all("PRAGMA table_info(requestLogs)", &[], |row| row.get::<_, String>(1))
        .unwrap();
    assert!(log_cols.contains(&"endpointKind".to_string()));
    assert_eq!(log_cols.last().unwrap(), "error");

    insert_request_log(
        &db,
        &json!({
            "timestamp": "2026-02-01T00:00:00.000Z",
            "apiKeyId": "legacy-key",
            "endpointKind": "chat",
            "model": "gpt-4o",
            "provider": "openai",
            "status": 200,
            "stream": true,
            "promptTokens": 1,
            "completionTokens": 2
        }),
    )
    .unwrap();
    // The legacy-shaped seed row is still readable next to the new one.
    let logs = get_request_logs(&db, &RequestLogFilter::from_json(None)).unwrap();
    let rows = logs["logs"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let inserted = rows
        .iter()
        .find(|r| r["apiKeyId"] == json!("legacy-key"))
        .expect("row written through insert_request_log");
    assert_eq!(inserted["endpointKind"], json!("chat"));
    assert_eq!(inserted["stream"], json!(true));
}

#[test]
fn rust_writes_are_readable_by_the_js_sql_shapes() {
    let dir = TempDir::new().unwrap();
    let (db, path) = open_with_js_file(&dir, false);

    // Write through the Rust repos…
    let created_key =
        create_api_key(&db, Some("from-rust".into()), "machine-abcd".into(), None).unwrap();
    create_combo(&db, &json!({"name": "rust-combo", "models": ["openai/gpt-4o"]})).unwrap();
    set_model_alias(&db, "alias-rust", "openai/gpt-4o").unwrap();
    save_request_detail(
        &db,
        &json!({"id": "det-rust", "timestamp": "2026-03-01T00:00:00.000Z", "provider": "openai", "model": "gpt-4o", "status": "ok"}),
    )
    .unwrap();
    insert_request_log(&db, &json!({"timestamp": "2026-03-01T00:00:00.000Z", "model": "gpt-4o", "status": 200}))
        .unwrap();

    // …then open the same file with plain SQL, the way the JS repos do, and
    // confirm the column names/values line up.
    let conn = rusqlite::Connection::open(&path).unwrap();
    let key: String = conn
        .query_row("SELECT key FROM apiKeys WHERE id = ?1", [&created_key.id], |r| r.get(0))
        .unwrap();
    assert_eq!(key, created_key.key);
    let access_restricted: i64 = conn
        .query_row("SELECT accessRestricted FROM apiKeys WHERE id = ?1", [&created_key.id], |r| r.get(0))
        .unwrap();
    assert_eq!(access_restricted, 0);

    let combo_models: String = conn
        .query_row("SELECT models FROM combos WHERE name = 'rust-combo'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(serde_json::from_str::<Value>(&combo_models).unwrap(), json!(["openai/gpt-4o"]));

    let alias: String = conn
        .query_row("SELECT value FROM kv WHERE scope='modelAliases' AND key='alias-rust'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(serde_json::from_str::<Value>(&alias).unwrap(), json!("openai/gpt-4o"));

    let detail_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM requestDetails WHERE id='det-rust'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(detail_rows, 1);
}

#[test]
fn rust_schema_ddl_matches_js_ddl() {
    // The Rust `TABLES` map must emit byte-identical DDL to the JS one, modulo
    // the `;` the JS printer omits. This is the real drift guard: a column added
    // on one side only fails here.
    let mut rust_stmts: Vec<String> = Vec::new();
    for (name, def) in nr_db::schema::tables().iter() {
        rust_stmts.push(nr_db::schema::build_create_table_sql(name, def));
        rust_stmts.extend(def.indexes.iter().cloned());
    }

    let js_stmts: Vec<String> = JS_SCHEMA_DDL
        .iter()
        .map(|s| s.trim_end_matches(';').to_string())
        .collect();

    assert_eq!(rust_stmts.len(), js_stmts.len(), "statement count drifted");
    for (rust, js) in rust_stmts.iter().zip(js_stmts.iter()) {
        assert_eq!(rust, js, "DDL drifted");
    }
}

#[test]
fn js_pragma_sql_is_applied() {
    let dir = TempDir::new().unwrap();
    let (db, _path) = open_with_js_file(&dir, false);
    let mode: String = db.query_row("PRAGMA journal_mode", &[], |r| r.get(0)).unwrap().unwrap();
    assert_eq!(mode.to_lowercase(), "wal");
    let fk: i64 = db.query_row("PRAGMA foreign_keys", &[], |r| r.get(0)).unwrap().unwrap();
    assert_eq!(fk, 1);
}

#[test]
fn export_shape_matches_js_export_db() {
    // src/lib/db/index.js exportDb() keys, in order.
    let dir = TempDir::new().unwrap();
    let (db, _path) = open_with_js_file(&dir, false);

    let exported = json!({
        "settings": nr_db::export_settings(&db).unwrap(),
        "providerConnections": get_provider_connections(&db, None).unwrap(),
        "providerNodes": get_provider_nodes(&db, None).unwrap(),
        "proxyPools": get_proxy_pools(&db, None).unwrap(),
        "apiKeys": get_api_keys(&db).unwrap().iter().map(|k| json!({
            "id": k.id, "key": k.key, "name": k.name, "machineId": k.machine_id,
            "isActive": k.is_active, "expiredAt": k.expired_at,
            "limitConcurrent": k.limit_concurrent, "rpm": k.rpm,
            "createdAt": k.created_at, "allowedModels": k.allowed_models,
            "allowedEndpoints": k.allowed_endpoints,
            "access": { "restricted": k.access.restricted, "allow": k.access.allow },
        })).collect::<Vec<_>>(),
        "combos": get_combos(&db).unwrap().iter().map(|c| json!({
            "id": c.id, "name": c.name, "kind": c.kind, "models": c.models,
            "createdAt": c.created_at, "updatedAt": c.updated_at,
        })).collect::<Vec<_>>(),
        "modelAliases": get_model_aliases(&db).unwrap(),
        "customModels": get_custom_models(&db).unwrap(),
        "mitmAlias": get_mitm_alias(&db, None).unwrap(),
        "pricing": get_pricing(&db).unwrap(),
    });

    let keys: Vec<&str> = exported.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec![
            "settings",
            "providerConnections",
            "providerNodes",
            "proxyPools",
            "apiKeys",
            "combos",
            "modelAliases",
            "customModels",
            "mitmAlias",
            "pricing",
        ]
    );
    // Values survive the round trip into JSON.
    assert_eq!(exported["apiKeys"][0]["key"], json!("sk-live-abcdefghijkl"));
    assert_eq!(exported["apiKeys"][0]["access"]["restricted"], json!(true));
}
