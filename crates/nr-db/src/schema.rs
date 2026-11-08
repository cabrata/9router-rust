/// Current schema version. Bump by +1 whenever you change the schema
/// (add/remove/alter a table, column, or index). Drives pre-change safety backup.
pub const SCHEMA_VERSION: i32 = 3;

/// PRAGMAs to set when opening a database connection
pub const PRAGMA_SQL: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA temp_store = MEMORY;
PRAGMA mmap_size = 30000000;
PRAGMA cache_size = -64000;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
"#;

/// Schema definition: table name → columns and indexes
#[derive(Clone)]
pub struct TableDef {
    pub columns: indexmap::IndexMap<String, String>,
    pub primary_key: Option<String>,
    pub indexes: Vec<String>,
}

pub fn tables() -> indexmap::IndexMap<String, TableDef> {
    let mut map = indexmap::IndexMap::new();

    // _meta: schema version tracking
    let mut cols = indexmap::IndexMap::new();
    cols.insert("key".to_string(), "TEXT PRIMARY KEY".to_string());
    cols.insert("value".to_string(), "TEXT NOT NULL".to_string());
    map.insert(
        "_meta".to_string(),
        TableDef {
            columns: cols,
            primary_key: None,
            indexes: vec![],
        },
    );

    // settings: global configuration
    let mut cols = indexmap::IndexMap::new();
    cols.insert("id".to_string(), "INTEGER PRIMARY KEY CHECK (id = 1)".to_string());
    cols.insert("data".to_string(), "TEXT NOT NULL".to_string());
    map.insert(
        "settings".to_string(),
        TableDef {
            columns: cols,
            primary_key: None,
            indexes: vec![],
        },
    );

    // providerConnections: OAuth/API accounts
    let mut cols = indexmap::IndexMap::new();
    cols.insert("id".to_string(), "TEXT PRIMARY KEY".to_string());
    cols.insert("provider".to_string(), "TEXT NOT NULL".to_string());
    cols.insert("authType".to_string(), "TEXT NOT NULL".to_string());
    cols.insert("name".to_string(), "TEXT".to_string());
    cols.insert("email".to_string(), "TEXT".to_string());
    cols.insert("priority".to_string(), "INTEGER".to_string());
    cols.insert("isActive".to_string(), "INTEGER DEFAULT 1".to_string());
    cols.insert("data".to_string(), "TEXT NOT NULL".to_string());
    cols.insert("createdAt".to_string(), "TEXT NOT NULL".to_string());
    cols.insert("updatedAt".to_string(), "TEXT NOT NULL".to_string());
    map.insert(
        "providerConnections".to_string(),
        TableDef {
            columns: cols,
            primary_key: None,
            indexes: vec![
                "CREATE INDEX IF NOT EXISTS idx_pc_provider ON providerConnections(provider)"
                    .to_string(),
                "CREATE INDEX IF NOT EXISTS idx_pc_provider_active ON providerConnections(provider, isActive)".to_string(),
                "CREATE INDEX IF NOT EXISTS idx_pc_priority ON providerConnections(provider, priority)".to_string(),
            ],
        },
    );

    // providerNodes: provider node configuration
    let mut cols = indexmap::IndexMap::new();
    cols.insert("id".to_string(), "TEXT PRIMARY KEY".to_string());
    cols.insert("type".to_string(), "TEXT".to_string());
    cols.insert("name".to_string(), "TEXT".to_string());
    cols.insert("data".to_string(), "TEXT NOT NULL".to_string());
    cols.insert("createdAt".to_string(), "TEXT NOT NULL".to_string());
    cols.insert("updatedAt".to_string(), "TEXT NOT NULL".to_string());
    map.insert(
        "providerNodes".to_string(),
        TableDef {
            columns: cols,
            primary_key: None,
            indexes: vec![
                "CREATE INDEX IF NOT EXISTS idx_pn_type ON providerNodes(type)".to_string(),
            ],
        },
    );

    // proxyPools: proxy configuration
    let mut cols = indexmap::IndexMap::new();
    cols.insert("id".to_string(), "TEXT PRIMARY KEY".to_string());
    cols.insert("isActive".to_string(), "INTEGER DEFAULT 1".to_string());
    cols.insert("testStatus".to_string(), "TEXT".to_string());
    cols.insert("data".to_string(), "TEXT NOT NULL".to_string());
    cols.insert("createdAt".to_string(), "TEXT NOT NULL".to_string());
    cols.insert("updatedAt".to_string(), "TEXT NOT NULL".to_string());
    map.insert(
        "proxyPools".to_string(),
        TableDef {
            columns: cols,
            primary_key: None,
            indexes: vec![
                "CREATE INDEX IF NOT EXISTS idx_pp_active ON proxyPools(isActive)".to_string(),
                "CREATE INDEX IF NOT EXISTS idx_pp_status ON proxyPools(testStatus)"
                    .to_string(),
            ],
        },
    );

    // apiKeys: API key management
    let mut cols = indexmap::IndexMap::new();
    cols.insert("id".to_string(), "TEXT PRIMARY KEY".to_string());
    cols.insert("key".to_string(), "TEXT UNIQUE NOT NULL".to_string());
    cols.insert("name".to_string(), "TEXT".to_string());
    cols.insert("machineId".to_string(), "TEXT".to_string());
    cols.insert("isActive".to_string(), "INTEGER DEFAULT 1".to_string());
    cols.insert("expiredAt".to_string(), "TEXT".to_string());
    cols.insert("limitConcurrent".to_string(), "INTEGER DEFAULT 0".to_string());
    cols.insert("rpm".to_string(), "INTEGER DEFAULT 0".to_string());
    cols.insert("createdAt".to_string(), "TEXT NOT NULL".to_string());
    cols.insert("allowedModels".to_string(), "TEXT".to_string());
    cols.insert("allowedEndpoints".to_string(), "TEXT".to_string());
    cols.insert("accessRestricted".to_string(), "INTEGER DEFAULT 0".to_string());
    cols.insert("accessAllow".to_string(), "TEXT".to_string());
    map.insert(
        "apiKeys".to_string(),
        TableDef {
            columns: cols,
            primary_key: None,
            indexes: vec!["CREATE INDEX IF NOT EXISTS idx_ak_key ON apiKeys(key)".to_string()],
        },
    );

    // combos: model combinations
    let mut cols = indexmap::IndexMap::new();
    cols.insert("id".to_string(), "TEXT PRIMARY KEY".to_string());
    cols.insert("name".to_string(), "TEXT UNIQUE NOT NULL".to_string());
    cols.insert("kind".to_string(), "TEXT".to_string());
    cols.insert("models".to_string(), "TEXT NOT NULL".to_string());
    cols.insert("createdAt".to_string(), "TEXT NOT NULL".to_string());
    cols.insert("updatedAt".to_string(), "TEXT NOT NULL".to_string());
    map.insert(
        "combos".to_string(),
        TableDef {
            columns: cols,
            primary_key: None,
            indexes: vec!["CREATE INDEX IF NOT EXISTS idx_combo_name ON combos(name)"
                .to_string()],
        },
    );

    // kv: key-value store (aliases, pricing, etc.)
    let mut cols = indexmap::IndexMap::new();
    cols.insert("scope".to_string(), "TEXT NOT NULL".to_string());
    cols.insert("key".to_string(), "TEXT NOT NULL".to_string());
    cols.insert("value".to_string(), "TEXT NOT NULL".to_string());
    map.insert(
        "kv".to_string(),
        TableDef {
            columns: cols,
            primary_key: Some("PRIMARY KEY (scope, key)".to_string()),
            indexes: vec![
                "CREATE INDEX IF NOT EXISTS idx_kv_scope ON kv(scope)".to_string(),
            ],
        },
    );

    // usageHistory: usage tracking
    let mut cols = indexmap::IndexMap::new();
    cols.insert("id".to_string(), "INTEGER PRIMARY KEY AUTOINCREMENT".to_string());
    cols.insert("timestamp".to_string(), "TEXT NOT NULL".to_string());
    cols.insert("provider".to_string(), "TEXT".to_string());
    cols.insert("model".to_string(), "TEXT".to_string());
    cols.insert("connectionId".to_string(), "TEXT".to_string());
    cols.insert("apiKey".to_string(), "TEXT".to_string());
    cols.insert("endpoint".to_string(), "TEXT".to_string());
    cols.insert("promptTokens".to_string(), "INTEGER DEFAULT 0".to_string());
    cols.insert("completionTokens".to_string(), "INTEGER DEFAULT 0".to_string());
    cols.insert("cost".to_string(), "REAL DEFAULT 0".to_string());
    cols.insert("status".to_string(), "TEXT".to_string());
    cols.insert("tokens".to_string(), "TEXT".to_string());
    cols.insert("meta".to_string(), "TEXT".to_string());
    map.insert(
        "usageHistory".to_string(),
        TableDef {
            columns: cols,
            primary_key: None,
            indexes: vec![
                "CREATE INDEX IF NOT EXISTS idx_uh_ts ON usageHistory(timestamp DESC)"
                    .to_string(),
                "CREATE INDEX IF NOT EXISTS idx_uh_provider ON usageHistory(provider)"
                    .to_string(),
                "CREATE INDEX IF NOT EXISTS idx_uh_model ON usageHistory(model)".to_string(),
                "CREATE INDEX IF NOT EXISTS idx_uh_conn ON usageHistory(connectionId)"
                    .to_string(),
            ],
        },
    );

    // usageDaily: daily usage summaries
    let mut cols = indexmap::IndexMap::new();
    cols.insert("dateKey".to_string(), "TEXT PRIMARY KEY".to_string());
    cols.insert("data".to_string(), "TEXT NOT NULL".to_string());
    map.insert(
        "usageDaily".to_string(),
        TableDef {
            columns: cols,
            primary_key: None,
            indexes: vec![],
        },
    );

    // requestDetails: detailed request information
    let mut cols = indexmap::IndexMap::new();
    cols.insert("id".to_string(), "TEXT PRIMARY KEY".to_string());
    cols.insert("timestamp".to_string(), "TEXT NOT NULL".to_string());
    cols.insert("provider".to_string(), "TEXT".to_string());
    cols.insert("model".to_string(), "TEXT".to_string());
    cols.insert("connectionId".to_string(), "TEXT".to_string());
    cols.insert("status".to_string(), "TEXT".to_string());
    cols.insert("data".to_string(), "TEXT NOT NULL".to_string());
    map.insert(
        "requestDetails".to_string(),
        TableDef {
            columns: cols,
            primary_key: None,
            indexes: vec![
                "CREATE INDEX IF NOT EXISTS idx_rd_ts ON requestDetails(timestamp DESC)"
                    .to_string(),
                "CREATE INDEX IF NOT EXISTS idx_rd_provider ON requestDetails(provider)"
                    .to_string(),
                "CREATE INDEX IF NOT EXISTS idx_rd_model ON requestDetails(model)".to_string(),
                "CREATE INDEX IF NOT EXISTS idx_rd_conn ON requestDetails(connectionId)"
                    .to_string(),
            ],
        },
    );

    // requestLogs: API request logging
    let mut cols = indexmap::IndexMap::new();
    cols.insert("id".to_string(), "INTEGER PRIMARY KEY AUTOINCREMENT".to_string());
    cols.insert("timestamp".to_string(), "TEXT NOT NULL".to_string());
    cols.insert("apiKeyId".to_string(), "TEXT".to_string());
    cols.insert("apiKeyName".to_string(), "TEXT".to_string());
    cols.insert("apiKeyMasked".to_string(), "TEXT".to_string());
    cols.insert("ip".to_string(), "TEXT".to_string());
    cols.insert("method".to_string(), "TEXT".to_string());
    cols.insert("path".to_string(), "TEXT".to_string());
    cols.insert("endpointKind".to_string(), "TEXT".to_string());
    cols.insert("model".to_string(), "TEXT".to_string());
    cols.insert("provider".to_string(), "TEXT".to_string());
    cols.insert("resolvedModel".to_string(), "TEXT".to_string());
    cols.insert("status".to_string(), "INTEGER".to_string());
    cols.insert("stream".to_string(), "INTEGER DEFAULT 0".to_string());
    cols.insert("promptTokens".to_string(), "INTEGER DEFAULT 0".to_string());
    cols.insert("completionTokens".to_string(), "INTEGER DEFAULT 0".to_string());
    cols.insert("durationMs".to_string(), "INTEGER DEFAULT 0".to_string());
    cols.insert("ttftMs".to_string(), "INTEGER DEFAULT 0".to_string());
    cols.insert("tps".to_string(), "REAL DEFAULT 0".to_string());
    cols.insert("userAgent".to_string(), "TEXT".to_string());
    cols.insert("error".to_string(), "TEXT".to_string());
    map.insert(
        "requestLogs".to_string(),
        TableDef {
            columns: cols,
            primary_key: None,
            indexes: vec![
                "CREATE INDEX IF NOT EXISTS idx_rl_ts ON requestLogs(timestamp DESC)"
                    .to_string(),
                "CREATE INDEX IF NOT EXISTS idx_rl_key ON requestLogs(apiKeyId)".to_string(),
            ],
        },
    );

    map
}

/// Build CREATE TABLE SQL from a table definition
pub fn build_create_table_sql(name: &str, def: &TableDef) -> String {
    let mut cols: Vec<String> = def
        .columns
        .iter()
        .map(|(k, v)| format!("{} {}", k, v))
        .collect();
    if let Some(pk) = &def.primary_key {
        cols.push(pk.clone());
    }
    format!("CREATE TABLE IF NOT EXISTS {} ({})", name, cols.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schema_version() {
        assert_eq!(SCHEMA_VERSION, 3);
    }

    #[test]
    fn test_tables_defined() {
        let tables = tables();
        assert!(tables.contains_key("_meta"));
        assert!(tables.contains_key("settings"));
        assert!(tables.contains_key("apiKeys"));
        assert!(tables.contains_key("usageHistory"));
    }

    #[test]
    fn test_build_create_table_sql() {
        let tables = tables();
        let meta = tables.get("_meta").unwrap();
        let sql = build_create_table_sql("_meta", meta);
        assert!(sql.contains("CREATE TABLE IF NOT EXISTS _meta"));
        assert!(sql.contains("key TEXT PRIMARY KEY"));
    }
}
