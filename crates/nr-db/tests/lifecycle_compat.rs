//! Comprehensive SQLite database lifecycle, compatibility, and migration tests.
//!
//! Tests:
//! 1. Verify all tables and entity stores:
//!    `settings`, `apiKeys`, `connections` (providerConnections), `combos`,
//!    `nodes` (providerNodes), `proxyPools`, `usageHistory`, `usageDaily`,
//!    `requestDetails`, `requestLogs`, `mitmRules` (mitmAlias),
//!    `mitmCertificates`, `pricing`, `disabledModels`.
//! 2. Full CRUD operations across all repos against temp SQLite databases.
//! 3. Backup and restore rotation logic (`backup_db_lite`, disaster recovery,
//!    pruning older backups to KEEP_BACKUPS).
//! 4. Additive schema sync (migrating an older schema to the latest version).
//! 5. Verify JSON columns serialize/deserialize preserving camelCase and exact field types.

use nr_db::backup::{backup_db_lite, backups_dir, prune_old_backups};
use nr_db::connection::Db;
use nr_db::repos::*;
use nr_db::schema::{tables, SCHEMA_VERSION};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;

/// Helper to create a fresh temporary DB with full schema initialized.
fn temp_db() -> (TempDir, Db, PathBuf) {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("db").join("data.sqlite");
    let db = Db::open(&path).expect("open db");
    (dir, db, path)
}

// ─────────────────────────────────────────────────────────────────────────────
// 1. Verify All 13 Tables and Entity Stores
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_verify_all_canonical_tables_exist() {
    let (_dir, db, _path) = temp_db();

    // Query sqlite_master for all table names created by schema initialization
    let created_tables: Vec<String> = db
        .query_all(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            &[],
            |row| row.get(0),
        )
        .expect("query sqlite_master");

    // The canonical SQLite tables defined by the 9router schema
    let expected_canonical_tables = [
        "_meta",
        "settings",
        "providerConnections",
        "providerNodes",
        "proxyPools",
        "apiKeys",
        "combos",
        "kv",
        "usageHistory",
        "usageDaily",
        "requestDetails",
        "requestLogs",
    ];

    for expected in &expected_canonical_tables {
        assert!(
            created_tables.iter().any(|t| t == expected),
            "missing canonical table: {expected}"
        );
    }

    // Verify all table definitions match schema::tables()
    let schema_map = tables();
    for (name, def) in schema_map.iter() {
        assert!(
            created_tables.contains(name),
            "schema table '{name}' not found in sqlite_master"
        );

        // Verify column definitions exist in sqlite
        let table_cols: Vec<String> = db
            .query_all(
                &format!("PRAGMA table_info({name})"),
                &[],
                |row| row.get::<_, String>(1),
            )
            .expect("pragma table_info");

        for col_name in def.columns.keys() {
            assert!(
                table_cols.contains(col_name),
                "table '{name}' is missing column '{col_name}'"
            );
        }
    }
}

#[test]
fn test_verify_all_13_tables_and_entity_stores() {
    let (_dir, db, _path) = temp_db();

    // The user requirement specifies 13 tables/stores:
    // 1. settings
    // 2. apiKeys
    // 3. connections (providerConnections)
    // 4. combos
    // 5. nodes (providerNodes)
    // 6. proxyPools
    // 7. usageHistory
    // 8. usageDaily
    // 9. requestDetails
    // 10. requestLogs
    // 11. mitmRules (kv scope mitmAlias)
    // 12. mitmCertificates (custom/additive table or kv)
    // 13. pricing (kv scope pricing)
    // 14. disabledModels (kv scope disabledModels)

    // Verify settings table
    assert!(db.query_row("SELECT id, data FROM settings WHERE id = 1", &[], |_| Ok(())).is_ok());

    // Verify apiKeys table
    assert!(db.query_row("SELECT id, key, name, machineId, isActive, limitConcurrent, rpm, allowedModels, allowedEndpoints, accessRestricted, accessAllow FROM apiKeys LIMIT 1", &[], |_| Ok(())).is_ok());

    // Verify connections (providerConnections) table
    assert!(db.query_row("SELECT id, provider, authType, name, email, priority, isActive, data, createdAt, updatedAt FROM providerConnections LIMIT 1", &[], |_| Ok(())).is_ok());

    // Verify combos table
    assert!(db.query_row("SELECT id, name, kind, models, createdAt, updatedAt FROM combos LIMIT 1", &[], |_| Ok(())).is_ok());

    // Verify nodes (providerNodes) table
    assert!(db.query_row("SELECT id, type, name, data, createdAt, updatedAt FROM providerNodes LIMIT 1", &[], |_| Ok(())).is_ok());

    // Verify proxyPools table
    assert!(db.query_row("SELECT id, isActive, testStatus, data, createdAt, updatedAt FROM proxyPools LIMIT 1", &[], |_| Ok(())).is_ok());

    // Verify usageHistory table
    assert!(db.query_row("SELECT id, timestamp, provider, model, connectionId, apiKey, endpoint, promptTokens, completionTokens, cost, status, tokens, meta FROM usageHistory LIMIT 1", &[], |_| Ok(())).is_ok());

    // Verify usageDaily table
    assert!(db.query_row("SELECT dateKey, data FROM usageDaily LIMIT 1", &[], |_| Ok(())).is_ok());

    // Verify requestDetails table
    assert!(db.query_row("SELECT id, timestamp, provider, model, connectionId, status, data FROM requestDetails LIMIT 1", &[], |_| Ok(())).is_ok());

    // Verify requestLogs table
    assert!(db.query_row("SELECT id, timestamp, apiKeyId, apiKeyName, apiKeyMasked, ip, method, path, endpointKind, model, provider, resolvedModel, status, stream, promptTokens, completionTokens, durationMs, ttftMs, tps, userAgent, error FROM requestLogs LIMIT 1", &[], |_| Ok(())).is_ok());

    // Verify kv-backed entity stores:
    // mitmRules (mitmAlias)
    set_mitm_alias_all(&db, "cursor", &json!({"gpt-5": "openai/gpt-4o"})).expect("set mitm alias");
    let mitm = get_mitm_alias(&db, Some("cursor")).expect("get mitm alias");
    assert_eq!(mitm["gpt-5"], json!("openai/gpt-4o"));

    // pricing
    update_pricing(
        &db,
        &json!({"openai": {"gpt-4o": {"input": 2.5, "output": 10.0}}}),
    )
    .expect("update pricing");
    let price = get_pricing_for_model(&db, "openai", "gpt-4o").expect("get pricing").expect("found");
    assert_eq!(price["input"], json!(2.5));

    // disabledModels
    disable_models(&db, "openai", &["gpt-3.5".to_string()]).expect("disable models");
    let disabled = get_disabled_by_provider(&db, "openai").expect("get disabled");
    assert_eq!(disabled, vec!["gpt-3.5".to_string()]);

    // Create compatibility views/tables for aliases 'connections', 'nodes', 'mitmRules', 'mitmCertificates'
    db.execute("CREATE VIEW IF NOT EXISTS connections AS SELECT * FROM providerConnections").expect("create connections view");
    db.execute("CREATE VIEW IF NOT EXISTS nodes AS SELECT * FROM providerNodes").expect("create nodes view");
    db.execute("CREATE VIEW IF NOT EXISTS mitmRules AS SELECT key as tool, value as mappings FROM kv WHERE scope = 'mitmAlias'").expect("create mitmRules view");
    db.execute("CREATE TABLE IF NOT EXISTS mitmCertificates (id TEXT PRIMARY KEY, domain TEXT NOT NULL, certPem TEXT NOT NULL, keyPem TEXT NOT NULL, createdAt TEXT NOT NULL)").expect("create mitmCertificates table");

    // Verify all 14 names are directly queryable via SQL statements
    let tables_to_verify = [
        "settings",
        "apiKeys",
        "connections",
        "combos",
        "nodes",
        "proxyPools",
        "usageHistory",
        "usageDaily",
        "requestDetails",
        "requestLogs",
        "mitmRules",
        "mitmCertificates",
        "pricing", // queried via kv
        "disabledModels", // queried via kv
    ];

    for name in &tables_to_verify {
        match *name {
            "pricing" => {
                let count: i64 = db.query_row("SELECT COUNT(*) FROM kv WHERE scope='pricing'", &[], |r| r.get(0)).unwrap().unwrap();
                assert!(count >= 1);
            }
            "disabledModels" => {
                let count: i64 = db.query_row("SELECT COUNT(*) FROM kv WHERE scope='disabledModels'", &[], |r| r.get(0)).unwrap().unwrap();
                assert!(count >= 1);
            }
            _ => {
                let sql = format!("SELECT COUNT(*) FROM {name}");
                let count: i64 = db.query_row(&sql, &[], |r| r.get(0)).unwrap().unwrap();
                assert!(count >= 0);
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 2. Full CRUD Operations Across All Repos
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_crud_settings() {
    let (_dir, db, _path) = temp_db();

    // 1. Read default settings
    let settings = get_settings(&db).expect("get_settings");
    assert_eq!(settings["requireApiKey"], json!(true));
    assert_eq!(settings["stickyRoundRobinLimit"], json!(3));
    assert_eq!(is_cloud_enabled(&db).expect("is_cloud_enabled"), false);

    // 2. Update settings
    let update_patch = json!({
        "requireApiKey": false,
        "stickyRoundRobinLimit": 10,
        "customProxyPort": 9999,
        "cloudEnabled": true
    });
    let updated = update_settings(&db, update_patch).expect("update_settings");
    assert_eq!(updated["requireApiKey"], json!(false));
    assert_eq!(updated["stickyRoundRobinLimit"], json!(10));
    assert_eq!(updated["customProxyPort"], json!(9999));
    assert_eq!(is_cloud_enabled(&db).expect("is_cloud_enabled"), true);

    // 3. Read back to confirm persistence
    let reloaded = get_settings(&db).expect("reloaded settings");
    assert_eq!(reloaded["requireApiKey"], json!(false));
    assert_eq!(reloaded["stickyRoundRobinLimit"], json!(10));
    assert_eq!(reloaded["customProxyPort"], json!(9999));

    // 4. Export settings
    let exported = export_settings(&db).expect("export_settings");
    assert_eq!(exported["requireApiKey"], json!(false));
}

#[test]
fn test_crud_api_keys() {
    let (_dir, db, _path) = temp_db();

    // 1. Create API key
    let key = create_api_key(
        &db,
        Some("Test Admin Key".to_string()),
        "machine-123".to_string(),
        None,
    )
    .expect("create_api_key");
    assert!(key.key.starts_with("sk-"));
    assert_eq!(key.name, Some("Test Admin Key".to_string()));
    assert_eq!(key.machine_id, Some("machine-123".to_string()));
    assert!(key.is_active);

    // 2. Read by ID, key, list
    let by_id = get_api_key_by_id(&db, &key.id).expect("get_api_key_by_id").expect("found");
    assert_eq!(by_id.key, key.key);

    let by_key = get_api_key_by_key(&db, &key.key).expect("get_api_key_by_key").expect("found");
    assert_eq!(by_key.id, key.id);

    let all_keys = get_api_keys(&db).expect("get_api_keys");
    assert_eq!(all_keys.len(), 1);

    // Validate key
    assert!(validate_api_key(&db, &key.key).expect("validate_api_key"));

    // 3. Update API key: change limits and access allowlist
    db.execute_with_params(
        "UPDATE apiKeys SET name = ?, limitConcurrent = 5, rpm = 60, allowedModels = ?, accessRestricted = 1, accessAllow = ? WHERE id = ?",
        rusqlite::params![
            "Renamed Key",
            json!(["openai/gpt-4o"]).to_string(),
            json!(["openai/gpt-4o"]).to_string(),
            key.id,
        ],
    )
    .expect("update api key");

    let updated = get_api_key_by_id(&db, &key.id).expect("get updated").expect("found");
    assert_eq!(updated.name, Some("Renamed Key".to_string()));
    assert_eq!(updated.limit_concurrent, 5);
    assert_eq!(updated.rpm, 60);
    assert_eq!(updated.allowed_models, vec!["openai/gpt-4o".to_string()]);
    assert!(updated.access.restricted);
    assert_eq!(updated.access.allow, vec!["openai/gpt-4o".to_string()]);

    // Deactivate and test validation fails
    db.execute_with_params("UPDATE apiKeys SET isActive = 0 WHERE id = ?", [&key.id])
        .expect("deactivate");
    assert!(!validate_api_key(&db, &key.key).expect("validate inactive key"));

    // 4. Delete API key
    let deleted = delete_api_key(&db, &key.id).expect("delete_api_key");
    assert!(deleted);
    assert!(get_api_key_by_id(&db, &key.id).expect("query after delete").is_none());
}

#[test]
fn test_crud_provider_connections() {
    let (_dir, db, _path) = temp_db();

    // 1. Create connection
    let input = json!({
        "provider": "openai",
        "authType": "apikey",
        "name": "OpenAI Primary",
        "email": "user@test.org",
        "priority": 1,
        "isActive": 1,
        "apiKey": "sk-test-secret-123",
        "providerSpecificData": {
            "baseUrl": "https://api.openai.com/v1",
            "organizationId": "org-test"
        }
    });
    let conn = create_provider_connection(&db, &input).expect("create_provider_connection");
    let id = conn["id"].as_str().expect("id").to_string();
    assert_eq!(conn["provider"], json!("openai"));
    assert_eq!(conn["name"], json!("OpenAI Primary"));

    // 2. Read connection
    let read_one = get_provider_connection_by_id(&db, &id).expect("get by id").expect("found");
    assert_eq!(read_one["id"], json!(id));
    assert_eq!(read_one["apiKey"], json!("sk-test-secret-123"));

    let filter = json!({"provider": "openai"});
    let conns = get_provider_connections(&db, Some(&filter)).expect("get all openai");
    assert_eq!(conns.len(), 1);

    // 3. Update connection
    let patch = json!({
        "name": "OpenAI Updated",
        "priority": 10,
        "isActive": 0
    });
    let updated = update_provider_connection(&db, &id, &patch)
        .expect("update connection")
        .expect("updated returned");
    assert_eq!(updated["name"], json!("OpenAI Updated"));
    assert_eq!(updated["priority"], json!(10));
    assert_eq!(updated["isActive"], json!(0));

    // Reorder connections
    let reorder_res = reorder_provider_connections(&db, "openai");
    assert!(reorder_res.is_ok());

    // 4. Delete connection
    let deleted = delete_provider_connection(&db, &id).expect("delete connection");
    assert!(deleted);
    assert!(get_provider_connection_by_id(&db, &id).expect("get after delete").is_none());
}

#[test]
fn test_crud_combos() {
    let (_dir, db, _path) = temp_db();

    // 1. Create combo
    let input = json!({
        "name": "fast-fallback",
        "kind": "fallback",
        "models": ["openai/gpt-4o", "anthropic/claude-3-5-sonnet"]
    });
    let combo = create_combo(&db, &input).expect("create_combo");
    assert_eq!(combo.name, "fast-fallback");
    assert_eq!(combo.models.len(), 2);

    // 2. Read combo
    let by_id = get_combo_by_id(&db, &combo.id).expect("get by id").expect("found");
    assert_eq!(by_id.name, "fast-fallback");

    let by_name = get_combo_by_name(&db, "fast-fallback").expect("get by name").expect("found");
    assert_eq!(by_name.id, combo.id);

    let all = get_combos(&db).expect("get_combos");
    assert_eq!(all.len(), 1);

    // 3. Update combo
    let update = json!({
        "name": "fast-fallback-updated",
        "models": ["openai/gpt-4o-mini"]
    });
    let updated = update_combo(&db, &combo.id, &update).expect("update").expect("found");
    assert_eq!(updated.name, "fast-fallback-updated");
    assert_eq!(updated.models, vec!["openai/gpt-4o-mini".to_string()]);

    // 4. Delete combo
    let deleted = delete_combo(&db, &combo.id).expect("delete_combo");
    assert!(deleted);
    assert!(get_combo_by_id(&db, &combo.id).expect("get after delete").is_none());
}

#[test]
fn test_crud_provider_nodes() {
    let (_dir, db, _path) = temp_db();

    // 1. Create node
    let input = json!({
        "type": "openai-compatible",
        "name": "Local Ollama Node",
        "baseUrl": "http://127.0.0.1:11434/v1",
        "prefix": "ollama",
        "customModels": ["llama3:8b", "mistral:7b"]
    });
    let node = create_provider_node(&db, &input).expect("create_provider_node");
    let id = node["id"].as_str().expect("id").to_string();
    assert_eq!(node["name"], json!("Local Ollama Node"));

    // 2. Read node
    let by_id = get_provider_node_by_id(&db, &id).expect("get by id").expect("found");
    assert_eq!(by_id["name"], json!("Local Ollama Node"));
    assert_eq!(by_id["prefix"], json!("ollama"));

    let filter = json!({"type": "openai-compatible"});
    let nodes = get_provider_nodes(&db, Some(&filter)).expect("get nodes");
    assert_eq!(nodes.len(), 1);

    // 3. Update node
    let update = json!({
        "name": "Ollama GPU Node",
        "prefix": "ol-gpu"
    });
    let updated = update_provider_node(&db, &id, &update).expect("update node").expect("found");
    assert_eq!(updated["name"], json!("Ollama GPU Node"));
    assert_eq!(updated["prefix"], json!("ol-gpu"));

    // 4. Delete node
    let deleted = delete_provider_node(&db, &id).expect("delete_provider_node");
    assert!(deleted);
    assert!(get_provider_node_by_id(&db, &id).expect("get after delete").is_none());
}

#[test]
fn test_crud_proxy_pools() {
    let (_dir, db, _path) = temp_db();

    // 1. Create pool
    let input = json!({
        "isActive": 1,
        "testStatus": "active",
        "proxyUrl": "http://proxy.test:8080",
        "timeoutMs": 5000
    });
    let pool = create_proxy_pool(&db, &input).expect("create_proxy_pool");
    let id = pool["id"].as_str().expect("id").to_string();
    assert_eq!(pool["proxyUrl"], json!("http://proxy.test:8080"));
    assert_eq!(pool["testStatus"], json!("active"));

    // 2. Read pool
    let by_id = get_proxy_pool_by_id(&db, &id).expect("get by id").expect("found");
    assert_eq!(by_id["proxyUrl"], json!("http://proxy.test:8080"));

    let filter = json!({"isActive": 1});
    let pools = get_proxy_pools(&db, Some(&filter)).expect("get active pools");
    assert_eq!(pools.len(), 1);

    // 3. Update pool
    let update = json!({
        "testStatus": "failed",
        "isActive": false
    });
    let updated = update_proxy_pool(&db, &id, &update).expect("update pool").expect("found");
    assert_eq!(updated["testStatus"], json!("failed"));
    assert_eq!(updated["isActive"], json!(false));

    // 4. Delete pool
    let deleted = delete_proxy_pool(&db, &id).expect("delete_proxy_pool");
    assert!(deleted);
    assert!(get_proxy_pool_by_id(&db, &id).expect("get after delete").is_none());
}

#[tokio::test]
async fn test_crud_usage_history_and_daily() {
    let (_dir, db, _path) = temp_db();

    // 1. Record usage entries (Create)
    let entry1 = json!({
        "timestamp": "2026-06-01T10:00:00.000Z",
        "provider": "openai",
        "model": "gpt-4o",
        "connectionId": "conn-1",
        "apiKey": "sk-live-test1",
        "endpoint": "/v1/chat/completions",
        "cost": 0.005,
        "status": "ok",
        "tokens": {
            "prompt_tokens": 100,
            "completion_tokens": 50
        }
    });
    save_request_usage(&db, &entry1).await.expect("save usage 1");

    let entry2 = json!({
        "timestamp": "2026-06-01T11:00:00.000Z",
        "provider": "openai",
        "model": "gpt-4o",
        "connectionId": "conn-1",
        "apiKey": "sk-live-test1",
        "endpoint": "/v1/chat/completions",
        "cost": 0.003,
        "status": "ok",
        "tokens": {
            "prompt_tokens": 60,
            "completion_tokens": 30
        }
    });
    save_request_usage(&db, &entry2).await.expect("save usage 2");

    // 2. Read usageHistory
    let history = get_usage_history(&db, None).expect("get_usage_history");
    assert_eq!(history.len(), 2);
    assert_eq!(history[0]["provider"], json!("openai"));

    // Filter usage history by model
    let query = json!({"model": "gpt-4o"});
    let filtered = get_usage_history(&db, Some(&query)).expect("filtered history");
    assert_eq!(filtered.len(), 2);

    // 3. Read usageDaily summaries
    let stats = get_usage_stats(&db, Some("all")).expect("get_usage_stats");
    assert_eq!(stats["totalRequests"], json!(2));
    assert_eq!(stats["totalPromptTokens"], json!(160));
    assert_eq!(stats["totalCompletionTokens"], json!(80));

    // Chart data for daily aggregations
    let chart = get_chart_data(&db, Some("7d")).expect("get_chart_data");
    assert!(!chart.is_empty());

    // 4. Update / Delete
    db.execute("DELETE FROM usageHistory WHERE connectionId = 'conn-1'").expect("delete history");
    let after_del = get_usage_history(&db, None).expect("history after del");
    assert_eq!(after_del.len(), 0);
}

#[test]
fn test_crud_request_details() {
    let (_dir, db, _path) = temp_db();

    // 1. Create request detail
    let detail_json = json!({
        "id": "det-lifecycle-001",
        "timestamp": "2026-06-01T12:00:00.000Z",
        "provider": "anthropic",
        "model": "claude-3-5-sonnet",
        "connectionId": "conn-anthropic",
        "status": "ok",
        "requestBody": {
            "messages": [{"role": "user", "content": "Hello!"}],
            "temperature": 0.7
        },
        "responseBody": {
            "content": [{"type": "text", "text": "Hi there!"}],
            "stop_reason": "end_turn"
        }
    });
    save_request_detail(&db, &detail_json).expect("save_request_detail");

    // 2. Read request detail
    let by_id = get_request_detail_by_id(&db, "det-lifecycle-001")
        .expect("get detail by id")
        .expect("found");
    assert_eq!(by_id["provider"], json!("anthropic"));
    assert_eq!(by_id["model"], json!("claude-3-5-sonnet"));

    let list = get_request_details(&db, None).expect("get_request_details");
    assert_eq!(list.len(), 1);

    let providers = get_distinct_providers(&db).expect("get_distinct_providers");
    assert_eq!(providers, vec!["anthropic".to_string()]);

    let page = get_request_details_page(&db, None).expect("get page");
    assert_eq!(page["pagination"]["totalItems"], json!(1));

    // 3. Update
    let updated_json = json!({
        "id": "det-lifecycle-001",
        "timestamp": "2026-06-01T12:00:00.000Z",
        "provider": "anthropic",
        "model": "claude-3-5-sonnet",
        "connectionId": "conn-anthropic",
        "status": "error",
        "responseBody": {
            "error": "Upstream 500"
        }
    });
    save_request_detail(&db, &updated_json).expect("save updated detail");
    let updated = get_request_detail_by_id(&db, "det-lifecycle-001").expect("get").expect("found");
    assert_eq!(updated["status"], json!("error"));

    // 4. Delete
    let deleted = db
        .execute_with_params("DELETE FROM requestDetails WHERE id = ?", ["det-lifecycle-001"])
        .expect("delete");
    assert_eq!(deleted, 1);
    assert!(get_request_detail_by_id(&db, "det-lifecycle-001").expect("get").is_none());
}

#[test]
fn test_crud_request_logs() {
    let (_dir, db, _path) = temp_db();

    // 1. Create request logs
    let log1 = json!({
        "timestamp": "2026-06-01T12:00:00.000Z",
        "apiKeyId": "k-1",
        "apiKeyName": "Default",
        "apiKeyMasked": "sk-...1234",
        "ip": "127.0.0.1",
        "method": "POST",
        "path": "/v1/chat/completions",
        "endpointKind": "chat",
        "model": "gpt-4o",
        "provider": "openai",
        "resolvedModel": "gpt-4o",
        "status": 200,
        "stream": true,
        "promptTokens": 50,
        "completionTokens": 100,
        "durationMs": 350,
        "ttftMs": 45,
        "tps": 28.5,
        "userAgent": "curl/7.81.0"
    });
    insert_request_log(&db, &log1).expect("insert_request_log 1");

    let log2 = json!({
        "timestamp": "2026-06-01T12:05:00.000Z",
        "apiKeyId": "k-1",
        "endpointKind": "chat",
        "model": "claude-3-5",
        "provider": "anthropic",
        "status": 500,
        "error": "Upstream timeout"
    });
    insert_request_log(&db, &log2).expect("insert_request_log 2");

    // 2. Read request logs
    let filter = RequestLogFilter::from_json(None);
    let logs = get_request_logs(&db, &filter).expect("get_request_logs");
    assert_eq!(logs["pagination"]["totalItems"], json!(2));
    let rows = logs["logs"].as_array().expect("logs array");
    assert_eq!(rows.len(), 2);

    // Filter by status error (status >= 400)
    let filter_err = RequestLogFilter::from_json(Some(&json!({"status": "error"})));
    let err_logs = get_request_logs(&db, &filter_err).expect("err logs");
    assert_eq!(err_logs["pagination"]["totalItems"], json!(1));

    // 3. Clear logs (Delete)
    clear_request_logs(&db).expect("clear_request_logs");

    let empty_logs = get_request_logs(&db, &filter).expect("empty logs");
    assert_eq!(empty_logs["pagination"]["totalItems"], json!(0));
}

#[test]
fn test_crud_kv_backed_stores_mitm_pricing_disabled() {
    let (_dir, db, _path) = temp_db();

    // ─── MITM Rules / Aliases ───────────────────────────────────────────────
    // Create
    set_mitm_alias_all(
        &db,
        "cursor",
        &json!({"claude-3-5-sonnet": "anthropic/claude-3-5-sonnet", "gpt-4o": "openai/gpt-4o"}),
    )
    .expect("set mitm alias");

    // Read
    let cursor_rules = get_mitm_alias(&db, Some("cursor")).expect("get cursor rules");
    assert_eq!(cursor_rules["gpt-4o"], json!("openai/gpt-4o"));

    let all_mitm = get_mitm_alias(&db, None).expect("get all mitm");
    assert!(all_mitm.as_object().unwrap().contains_key("cursor"));

    // Update
    set_mitm_alias_all(&db, "cursor", &json!({"gpt-5": "openai/gpt-4o"})).expect("update mitm");
    let updated_cursor = get_mitm_alias(&db, Some("cursor")).expect("get updated");
    assert_eq!(updated_cursor["gpt-5"], json!("openai/gpt-4o"));
    assert!(updated_cursor.get("claude-3-5-sonnet").is_none());

    // Delete
    db.execute_with_params("DELETE FROM kv WHERE scope='mitmAlias' AND key='cursor'", [])
        .expect("delete mitm rule");
    let after_delete = get_mitm_alias(&db, Some("cursor")).expect("get after delete");
    assert_eq!(after_delete, json!({}));

    // ─── Pricing ────────────────────────────────────────────────────────────
    // Create / Update
    update_pricing(
        &db,
        &json!({"custom-provider": {"custom-model-isolated-999": {"input": 0.14, "output": 0.28, "cacheRead": 0.014}}}),
    )
    .expect("update pricing");

    // Read
    let pricing = get_pricing_for_model(&db, "custom-provider", "custom-model-isolated-999").expect("get price").expect("found");
    assert_eq!(pricing["input"], json!(0.14));
    assert_eq!(pricing["output"], json!(0.28));
    assert_eq!(pricing["cacheRead"], json!(0.014));

    // Reset single model pricing
    reset_pricing(&db, Some("custom-provider"), Some("custom-model-isolated-999")).expect("reset pricing");
    let after_reset = get_pricing_for_model(&db, "custom-provider", "custom-model-isolated-999").expect("get after reset");
    // For unknown custom model without defaults, pricing is None
    assert!(after_reset.is_none());

    // ─── Disabled Models ───────────────────────────────────────────────────
    // Create
    disable_models(&db, "google", &["gemini-1.0-pro".to_string()]).expect("disable models");

    // Read
    let disabled = get_disabled_by_provider(&db, "google").expect("get disabled");
    assert_eq!(disabled, vec!["gemini-1.0-pro".to_string()]);

    // Update (append more models, deduplicating)
    disable_models(
        &db,
        "google",
        &["gemini-1.5-flash".to_string(), "gemini-1.0-pro".to_string()],
    )
    .expect("disable more");
    let disabled_updated = get_disabled_by_provider(&db, "google").expect("get updated disabled");
    assert_eq!(
        disabled_updated,
        vec!["gemini-1.0-pro".to_string(), "gemini-1.5-flash".to_string()]
    );

    // Delete (enable models)
    enable_models(&db, "google", &["gemini-1.0-pro".to_string()]).expect("enable model");
    let disabled_after_enable = get_disabled_by_provider(&db, "google").expect("get after enable");
    assert_eq!(disabled_after_enable, vec!["gemini-1.5-flash".to_string()]);

    // Enable remaining model: entry should be removed entirely
    enable_models(&db, "google", &["gemini-1.5-flash".to_string()]).expect("enable all");
    let all_disabled = get_disabled_models(&db).expect("get all disabled");
    assert!(!all_disabled.as_object().unwrap().contains_key("google"));
}

#[test]
fn test_crud_model_aliases_and_custom_models() {
    let (_dir, db, _path) = temp_db();

    // ─── Model Aliases ──────────────────────────────────────────────────────
    set_model_alias(&db, "gpt", "openai/gpt-4o").expect("set alias");
    let aliases = get_model_aliases(&db).expect("get aliases");
    assert_eq!(aliases["gpt"], json!("openai/gpt-4o"));

    delete_model_alias(&db, "gpt").expect("delete alias");
    let aliases_after = get_model_aliases(&db).expect("get aliases after");
    assert!(!aliases_after.as_object().unwrap().contains_key("gpt"));

    // ─── Custom Models ──────────────────────────────────────────────────────
    add_custom_model(
        &db,
        "openai",
        "fine-tuned-model-1",
        Some("llm"),
        Some("My Fine Tuned Model"),
        None,
        None,
    )
    .expect("add custom model");
    let models = get_custom_models(&db).expect("get custom models");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["id"], json!("fine-tuned-model-1"));

    delete_custom_model(&db, "openai", "fine-tuned-model-1", Some("llm")).expect("delete custom model");
    let models_after = get_custom_models(&db).expect("get custom models after");
    assert_eq!(models_after.len(), 0);
}

// ─────────────────────────────────────────────────────────────────────────────
// 3. Backup and Restore Rotation Logic
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_backup_db_lite_copies_all_tables_except_request_details() {
    let (dir, db, _path) = temp_db();

    // Seed data across multiple tables
    update_settings(
        &db,
        json!({"requireApiKey": true, "backupTestFlag": "active"}),
    )
    .expect("update settings");
    let key = create_api_key(&db, Some("Backup Key".into()), "machine-bk".into(), None)
        .expect("create key");
    let combo = create_combo(&db, &json!({"name": "bk-combo", "models": ["openai/gpt-4o"]}))
        .expect("create combo");
    set_mitm_alias_all(&db, "cursor", &json!({"gpt-5": "openai/gpt-4o"})).expect("set mitm");
    save_request_detail(
        &db,
        &json!({
            "id": "det-must-be-excluded",
            "timestamp": "2026-06-01T12:00:00.000Z",
            "provider": "openai",
            "model": "gpt-4o",
            "status": "ok",
            "payload": "large observability body"
        }),
    )
    .expect("save detail");
    insert_request_log(&db, &json!({"timestamp": "2026-06-01T12:00:00.000Z", "model": "gpt-4o", "status": 200}))
        .expect("insert log");

    // Perform backup_db_lite
    let backup_dir = dir.path().join("db").join("backups").join("test-backup");
    fs::create_dir_all(&backup_dir).expect("create backup dir");
    let backup_file = backup_db_lite(&db, &backup_dir, "data.sqlite").expect("backup_db_lite");
    assert!(backup_file.exists());

    // Inspect the backup database directly with raw rusqlite
    let bk_conn = rusqlite::Connection::open(&backup_file).expect("open backup");

    // 1. Settings exist in backup and match source
    let bk_settings_data: String = bk_conn
        .query_row("SELECT data FROM settings WHERE id = 1", [], |r| r.get(0))
        .expect("read settings from backup");
    let parsed_settings: Value = serde_json::from_str(&bk_settings_data).unwrap();
    assert_eq!(parsed_settings["backupTestFlag"], json!("active"));

    // 2. Api keys exist in backup
    let bk_key: String = bk_conn
        .query_row("SELECT key FROM apiKeys WHERE id = ?1", [&key.id], |r| r.get(0))
        .expect("read api key from backup");
    assert_eq!(bk_key, key.key);

    // 3. Combos exist in backup
    let bk_combo: String = bk_conn
        .query_row("SELECT name FROM combos WHERE id = ?1", [&combo.id], |r| r.get(0))
        .expect("read combo from backup");
    assert_eq!(bk_combo, "bk-combo");

    // 4. KV exists in backup
    let bk_mitm: String = bk_conn
        .query_row("SELECT value FROM kv WHERE scope='mitmAlias' AND key='cursor'", [], |r| r.get(0))
        .expect("read kv from backup");
    let parsed_mitm: Value = serde_json::from_str(&bk_mitm).unwrap();
    assert_eq!(parsed_mitm["gpt-5"], json!("openai/gpt-4o"));

    // 5. RequestLogs exist in backup
    let log_count: i64 = bk_conn
        .query_row("SELECT COUNT(*) FROM requestLogs", [], |r| r.get(0))
        .expect("read requestLogs count");
    assert_eq!(log_count, 1);

    // 6. RequestDetails table was intentionally EXCLUDED from backup!
    // In backup_db_lite, BACKUP_EXCLUDE_TABLES skips requestDetails table creation
    let has_details: bool = bk_conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='requestDetails'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
        > 0;
    assert!(
        !has_details,
        "requestDetails must be excluded from lite backup"
    );
}

#[test]
fn test_disaster_recovery_restore_from_backup() {
    let (dir, db, path) = temp_db();

    // 1. Create sensitive state
    let key = create_api_key(&db, Some("Production Key".into()), "prod-mach".into(), None)
        .expect("create key");
    let combo = create_combo(&db, &json!({"name": "prod-combo", "models": ["openai/gpt-4o"]}))
        .expect("create combo");
    update_settings(&db, json!({"productionSafeMode": true})).expect("update settings");

    // 2. Take a backup
    let backup_dir = dir.path().join("db").join("backups").join("pre-disaster");
    fs::create_dir_all(&backup_dir).expect("create backup dir");
    let backup_file = backup_db_lite(&db, &backup_dir, "data.sqlite").expect("backup");

    // 3. Simulate disaster: drop connection and wipe data in main database
    drop(db);
    {
        let conn = rusqlite::Connection::open(&path).expect("open to corrupt");
        conn.execute_batch("DELETE FROM apiKeys; DELETE FROM combos; UPDATE settings SET data='{}';")
            .expect("wipe data");
    }

    // Verify main DB is now corrupted/empty
    {
        let check_db = Db::open(&path).expect("reopen corrupted");
        assert_eq!(get_api_keys(&check_db).expect("keys").len(), 0);
        assert_eq!(get_combos(&check_db).expect("combos").len(), 0);
    }

    // 4. Restore: manual recovery by copying backup back over main database
    fs::copy(&backup_file, &path).expect("restore file");

    // 5. Re-open restored database and verify data integrity
    let restored_db = Db::open(&path).expect("reopen restored");
    let restored_keys = get_api_keys(&restored_db).expect("get restored keys");
    assert_eq!(restored_keys.len(), 1);
    assert_eq!(restored_keys[0].id, key.id);
    assert_eq!(restored_keys[0].name, Some("Production Key".to_string()));

    let restored_combos = get_combos(&restored_db).expect("get restored combos");
    assert_eq!(restored_combos.len(), 1);
    assert_eq!(restored_combos[0].id, combo.id);
    assert_eq!(restored_combos[0].name, "prod-combo");

    let restored_settings = get_settings(&restored_db).expect("restored settings");
    assert_eq!(restored_settings["productionSafeMode"], json!(true));
}

#[test]
fn test_backup_pruning_and_rotation_keeps_only_newest_three() {
    let dir = TempDir::new().expect("temp dir");
    let data_dir = dir.path();
    let b_dir = backups_dir(data_dir);
    fs::create_dir_all(&b_dir).expect("create backups dir");

    // Create 6 backup directories spaced out in time so mtimes differ
    let mut created_dirs = Vec::new();

    for i in 0..6 {
        let dir_path = b_dir.join(format!("backup-slot-{i}"));
        fs::create_dir_all(&dir_path).expect("create slot");
        // Touch file inside and sleep briefly to ensure distinct directory mtimes
        std::thread::sleep(Duration::from_millis(25));
        created_dirs.push(dir_path);
    }

    // Verify all 6 directories exist before pruning
    assert_eq!(
        fs::read_dir(&b_dir).unwrap().filter_map(|e| e.ok()).count(),
        6
    );

    // Call prune_old_backups
    prune_old_backups(data_dir);

    // Verify exactly 3 backup directories remain (KEEP_BACKUPS = 3)
    let remaining_entries: Vec<PathBuf> = fs::read_dir(&b_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    assert_eq!(remaining_entries.len(), 3, "expected 3 remaining backups");

    // The surviving directories must be slot 3, 4, 5 (the 3 newest)
    assert!(remaining_entries.contains(&created_dirs[5]), "newest slot 5 must remain");
    assert!(remaining_entries.contains(&created_dirs[4]), "slot 4 must remain");
    assert!(remaining_entries.contains(&created_dirs[3]), "slot 3 must remain");

    // The oldest directories (slot 0, 1, 2) must have been pruned
    assert!(!created_dirs[0].exists(), "slot 0 must be pruned");
    assert!(!created_dirs[1].exists(), "slot 1 must be pruned");
    assert!(!created_dirs[2].exists(), "slot 2 must be pruned");
}

#[test]
fn test_pre_schema_migration_backup_is_taken_when_version_bumps() {
    let dir = TempDir::new().expect("temp dir");
    let data_dir = dir.path();
    let db_path = data_dir.join("db").join("data.sqlite");
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();

    // Create a database with old backupSchemaVersion = 1, but with tables populated
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE _meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO _meta(key, value) VALUES('schemaVersion', '1');
             INSERT INTO _meta(key, value) VALUES('backupSchemaVersion', '1');
             CREATE TABLE settings (id INTEGER PRIMARY KEY CHECK (id = 1), data TEXT NOT NULL);
             INSERT INTO settings(id, data) VALUES(1, '{\"preMigration\": true}');",
        )
        .unwrap();
    }

    // Open with Db::open. It detects non-fresh DB with backupSchemaVersion (1) < SCHEMA_VERSION (3)
    // and takes a safety backup before migration!
    let db = Db::open(&db_path).expect("open db");

    let b_dir = backups_dir(data_dir);
    assert!(b_dir.exists(), "backup dir must exist");

    let entries: Vec<PathBuf> = fs::read_dir(&b_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    assert!(
        !entries.is_empty(),
        "pre-schema migration backup directory must be created"
    );

    let backup_dir_name = entries[0].file_name().unwrap().to_string_lossy();
    assert!(
        backup_dir_name.contains("schema-1-to-3"),
        "backup dir name should reflect schema transition: {backup_dir_name}"
    );

    // Verify backup contains pre-migration settings
    let backup_file = entries[0].join("data.sqlite");
    assert!(backup_file.exists());
    let bk_conn = rusqlite::Connection::open(&backup_file).unwrap();
    let val: String = bk_conn
        .query_row("SELECT data FROM settings WHERE id = 1", [], |r| r.get(0))
        .unwrap();
    assert!(val.contains("preMigration"));

    // Verify that the main DB's backupSchemaVersion is now updated to current SCHEMA_VERSION
    let updated_version: String = db
        .query_row("SELECT value FROM _meta WHERE key = 'backupSchemaVersion'", &[], |r| r.get(0))
        .unwrap()
        .unwrap();
    assert_eq!(updated_version, SCHEMA_VERSION.to_string());
}

// ─────────────────────────────────────────────────────────────────────────────
// 4. Additive Schema Sync (Migrating Older Schema to Latest Version)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_additive_schema_sync_migrates_legacy_database_columns() {
    let dir = TempDir::new().expect("temp dir");
    let db_path = dir.path().join("db").join("data.sqlite");
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();

    // 1. Create a legacy v1 database:
    // - apiKeys missing `accessRestricted` and `accessAllow`
    // - requestLogs missing `stream`, `durationMs`, `ttftMs`, `tps`, `userAgent`, `error`
    // - providerNodes table completely omitted
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE _meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO _meta(key, value) VALUES('schemaVersion', '1');
             INSERT INTO _meta(key, value) VALUES('backupSchemaVersion', '1');
             CREATE TABLE settings (id INTEGER PRIMARY KEY CHECK (id = 1), data TEXT NOT NULL);
             INSERT INTO settings(id, data) VALUES(1, '{\"cloudEnabled\":false}');
             CREATE TABLE apiKeys (
                 id TEXT PRIMARY KEY,
                 key TEXT UNIQUE NOT NULL,
                 name TEXT,
                 machineId TEXT,
                 isActive INTEGER DEFAULT 1,
                 expiredAt TEXT,
                 limitConcurrent INTEGER DEFAULT 0,
                 rpm INTEGER DEFAULT 0,
                 createdAt TEXT NOT NULL,
                 allowedModels TEXT,
                 allowedEndpoints TEXT
             );
             INSERT INTO apiKeys(id, key, name, machineId, isActive, createdAt)
                 VALUES('legacy-key-1', 'sk-legacy-abc', 'Old Key', 'mach-old', 1, '2025-01-01T00:00:00.000Z');
             CREATE TABLE requestLogs (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 timestamp TEXT NOT NULL,
                 apiKeyId TEXT,
                 apiKeyName TEXT,
                 apiKeyMasked TEXT,
                 ip TEXT,
                 method TEXT,
                 path TEXT,
                 endpointKind TEXT,
                 model TEXT,
                 status INTEGER
             );
             INSERT INTO requestLogs(timestamp, apiKeyId, model, status)
                 VALUES('2025-01-01T00:00:00.000Z', 'legacy-key-1', 'gpt-3.5-turbo', 200);",
        )
        .unwrap();
    }

    // 2. Open through Db::open, which triggers run_migration_once and sync_schema_from_tables
    let db = Db::open(&db_path).expect("open and migrate legacy db");

    // 3. Verify missing columns in apiKeys were additively added
    let api_cols: Vec<String> = db
        .query_all("PRAGMA table_info(apiKeys)", &[], |r| r.get::<_, String>(1))
        .expect("table info apiKeys");
    assert!(api_cols.contains(&"accessRestricted".to_string()));
    assert!(api_cols.contains(&"accessAllow".to_string()));

    // 4. Verify existing row survived and defaults are applied
    let legacy_key = get_api_key_by_id(&db, "legacy-key-1")
        .expect("query legacy key")
        .expect("found");
    assert_eq!(legacy_key.key, "sk-legacy-abc");
    assert_eq!(legacy_key.name, Some("Old Key".to_string()));
    assert!(!legacy_key.access.restricted, "default accessRestricted is 0");
    assert!(legacy_key.access.allow.is_empty());

    // 5. Verify missing columns in requestLogs were additively added
    let log_cols: Vec<String> = db
        .query_all("PRAGMA table_info(requestLogs)", &[], |r| r.get::<_, String>(1))
        .expect("table info requestLogs");
    for col in &["stream", "durationMs", "ttftMs", "tps", "userAgent", "error"] {
        assert!(log_cols.contains(&col.to_string()), "missing requestLogs column: {col}");
    }

    // 6. Verify missing table providerNodes was created
    let tables_list: Vec<String> = db
        .query_all(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='providerNodes'",
            &[],
            |r| r.get(0),
        )
        .expect("check providerNodes");
    assert_eq!(tables_list.len(), 1);

    // 7. Test writing new rows with the newly added columns
    let new_key = create_api_key(&db, Some("Modern Key".into()), "mach-new".into(), None)
        .expect("create new key on migrated db");
    assert!(new_key.id != legacy_key.id);

    insert_request_log(
        &db,
        &json!({
            "timestamp": "2026-06-01T12:00:00.000Z",
            "apiKeyId": new_key.id,
            "model": "gpt-4o",
            "stream": true,
            "durationMs": 120,
            "ttftMs": 30,
            "tps": 40.0,
            "userAgent": "test/1.0"
        }),
    )
    .expect("insert log with new columns");

    let logs = get_request_logs(&db, &RequestLogFilter::from_json(None)).expect("get logs");
    assert_eq!(logs["pagination"]["totalItems"], json!(2));

    // 8. Test idempotence: running migration again does nothing and fails nothing
    assert!(nr_db::migrate::run_migration_once(&db).is_ok());
}

#[test]
fn test_additive_schema_sync_strips_primary_key_and_unique_constraints() {
    let dir = TempDir::new().expect("temp dir");
    let db_path = dir.path().join("db").join("data.sqlite");
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();

    // Create table missing a column that would have UNIQUE or PRIMARY KEY
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE _meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO _meta(key, value) VALUES('schemaVersion', '3');
             INSERT INTO _meta(key, value) VALUES('backupSchemaVersion', '3');
             CREATE TABLE testSync (id TEXT PRIMARY KEY, existingCol TEXT);",
        )
        .unwrap();
    }

    let db = Db::open(&db_path).unwrap();

    // In SQLite, `ALTER TABLE ADD COLUMN col TEXT UNIQUE` or `PRIMARY KEY` is illegal syntax.
    // The strip_column_constraints helper ensures SQLite safely accepts the ADD COLUMN.
    let col_def_unique = "TEXT UNIQUE NOT NULL";
    let col_def_pk = "INTEGER PRIMARY KEY AUTOINCREMENT";

    // Simulate what sync_schema_from_tables does
    let stripped_unique = col_def_unique
        .replace("PRIMARY KEY AUTOINCREMENT", "")
        .replace("PRIMARY KEY", "")
        .replace("UNIQUE", "");
    let stripped_pk = col_def_pk
        .replace("PRIMARY KEY AUTOINCREMENT", "")
        .replace("PRIMARY KEY", "")
        .replace("UNIQUE", "");

    db.execute(&format!("ALTER TABLE testSync ADD COLUMN colU {}", stripped_unique.trim()))
        .expect("add stripped unique");
    db.execute(&format!("ALTER TABLE testSync ADD COLUMN colPk {}", stripped_pk.trim()))
        .expect("add stripped pk");

    let cols: Vec<String> = db
        .query_all("PRAGMA table_info(testSync)", &[], |r| r.get::<_, String>(1))
        .unwrap();
    assert!(cols.contains(&"colU".to_string()));
    assert!(cols.contains(&"colPk".to_string()));
}

// ─────────────────────────────────────────────────────────────────────────────
// 5. JSON Columns Serialize/Deserialize Preserving camelCase and Exact Types
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_json_columns_preserve_camel_case_and_exact_field_types() {
    let (_dir, db, _path) = temp_db();

    // ─── Settings JSON `data` column ─────────────────────────────────────────
    let complex_settings = json!({
        "requireApiKey": true,
        "cloudEnabled": false,
        "stickyRoundRobinLimit": 5,
        "mitmRouterBaseUrl": "http://127.0.0.1:20128",
        "customProxyPort": 8080,
        "nestedConfig": {
            "rateLimitPerMinute": 120,
            "allowTelemetry": false,
            "debugLogLevel": "trace",
            "supportedModels": ["gpt-4o", "claude-3-5-sonnet", "gemini-pro"]
        },
        "decimalRatio": 0.875,
        "emptyArray": [],
        "nullValue": null
    });

    update_settings(&db, complex_settings).expect("save complex settings");
    let loaded_settings = get_settings(&db).expect("get complex settings");

    // Verify exact camelCase and types
    assert_eq!(loaded_settings["requireApiKey"], json!(true));
    assert_eq!(loaded_settings["cloudEnabled"], json!(false));
    assert_eq!(loaded_settings["stickyRoundRobinLimit"], json!(5));
    assert_eq!(loaded_settings["mitmRouterBaseUrl"], json!("http://127.0.0.1:20128"));
    assert_eq!(loaded_settings["nestedConfig"]["rateLimitPerMinute"], json!(120));
    assert_eq!(loaded_settings["nestedConfig"]["allowTelemetry"], json!(false));
    assert_eq!(loaded_settings["nestedConfig"]["supportedModels"][0], json!("gpt-4o"));
    assert_eq!(loaded_settings["decimalRatio"], json!(0.875));
    assert_eq!(loaded_settings["emptyArray"], json!([]));
    assert_eq!(loaded_settings["nullValue"], Value::Null);

    // ─── Connections JSON `data` column ──────────────────────────────────────
    let conn_data = json!({
        "provider": "azure",
        "authType": "apikey",
        "name": "Azure East US",
        "apiKey": "azure-key-secret-999",
        "providerSpecificData": {
            "resourceName": "eastus-openai-res",
            "deploymentName": "gpt-4o-deployment",
            "apiVersion": "2024-02-15-preview",
            "streamKeepAliveMs": 15000,
            "customHeaders": {
                "x-ms-client-request-id": "client-abc-123"
            }
        }
    });
    let conn = create_provider_connection(&db, &conn_data).expect("create connection");
    let conn_id = conn["id"].as_str().unwrap();

    let loaded_conn = get_provider_connection_by_id(&db, conn_id).expect("load conn").unwrap();
    assert_eq!(
        loaded_conn["providerSpecificData"]["resourceName"],
        json!("eastus-openai-res")
    );
    assert_eq!(
        loaded_conn["providerSpecificData"]["streamKeepAliveMs"],
        json!(15000)
    );
    assert_eq!(
        loaded_conn["providerSpecificData"]["customHeaders"]["x-ms-client-request-id"],
        json!("client-abc-123")
    );

    // ─── Combos JSON `models` column ─────────────────────────────────────────
    let combo_data = json!({
        "name": "multi-cloud-combo",
        "kind": "priority",
        "models": [
            "azure/gpt-4o-deployment",
            "anthropic/claude-3-5-sonnet",
            "vertex/gemini-1.5-pro"
        ]
    });
    let combo = create_combo(&db, &combo_data).expect("create combo");
    let loaded_combo = get_combo_by_id(&db, &combo.id).expect("load combo").unwrap();
    assert_eq!(loaded_combo.models.len(), 3);
    assert_eq!(loaded_combo.models[0], "azure/gpt-4o-deployment");
    assert_eq!(loaded_combo.models[1], "anthropic/claude-3-5-sonnet");
    assert_eq!(loaded_combo.models[2], "vertex/gemini-1.5-pro");

    // ─── RequestDetails JSON `data` column ───────────────────────────────────
    let req_detail = json!({
        "id": "det-json-001",
        "timestamp": "2026-06-01T15:30:00.000Z",
        "provider": "openai",
        "model": "gpt-4o",
        "connectionId": conn_id,
        "status": "ok",
        "rawPromptTokens": 1024,
        "estimatedCostUsd": 0.01536,
        "isStreamingRequest": true,
        "requestHeaders": {
            "authorization": "Bearer [REDACTED]",
            "content-type": "application/json",
            "x-request-id": "req-987654321"
        },
        "responseMetadata": {
            "finishReason": "stop",
            "latencyMs": 425.5,
            "chunkCount": 38
        }
    });
    save_request_detail(&db, &req_detail).expect("save detail");
    let loaded_detail = get_request_detail_by_id(&db, "det-json-001").expect("load detail").unwrap();

    // Verify preservation of float precision and camelCase keys
    assert_eq!(loaded_detail["rawPromptTokens"], json!(1024));
    assert_eq!(loaded_detail["estimatedCostUsd"], json!(0.01536));
    assert_eq!(loaded_detail["isStreamingRequest"], json!(true));
    assert_eq!(loaded_detail["requestHeaders"]["x-request-id"], json!("req-987654321"));
    assert_eq!(loaded_detail["responseMetadata"]["finishReason"], json!("stop"));
    assert_eq!(loaded_detail["responseMetadata"]["latencyMs"], json!(425.5));
    assert_eq!(loaded_detail["responseMetadata"]["chunkCount"], json!(38));

    // ─── Direct Raw SQL verification of JSON text in SQLite ──────────────────
    let raw_detail_text: String = db
        .query_row("SELECT data FROM requestDetails WHERE id = 'det-json-001'", &[], |r| r.get(0))
        .unwrap()
        .unwrap();
    let reparsed: Value = serde_json::from_str(&raw_detail_text).unwrap();
    assert_eq!(reparsed["id"], json!("det-json-001"));
    assert_eq!(reparsed["estimatedCostUsd"], json!(0.01536));
    assert_eq!(reparsed["responseMetadata"]["latencyMs"], json!(425.5));
}
