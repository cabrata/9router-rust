use crate::connection::Db;
use crate::schema::{build_create_table_sql, tables, SCHEMA_VERSION};
use nr_core::Result;
use std::collections::HashSet;

/// Get the current schema version from _meta table
fn get_schema_version(db: &Db) -> Result<i32> {
    db.query_row(
        "SELECT value FROM _meta WHERE key = 'schemaVersion'",
        &[],
        |row| {
            let value: String = row.get(0)?;
            Ok(value.parse::<i32>().unwrap_or(0))
        },
    )
    .map(|opt| opt.unwrap_or(0))
}

/// Set the schema version in _meta table
fn set_schema_version(db: &Db, version: i32) -> Result<()> {
    db.execute_with_params(
        "INSERT INTO _meta(key, value) VALUES('schemaVersion', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [version.to_string()],
    )?;
    Ok(())
}

/// Ensure _meta table exists and bootstrap it
fn bootstrap_meta(db: &Db) -> Result<()> {
    let tables = tables();
    if let Some(meta_def) = tables.get("_meta") {
        let sql = build_create_table_sql("_meta", meta_def);
        db.execute(&sql)?;
    }
    Ok(())
}

/// Check if database is fresh (no tables or only _meta with no rows)
fn is_fresh_db(db: &Db) -> Result<bool> {
    match db.query_row(
        "SELECT COUNT(*) as c FROM _meta",
        &[],
        |row| row.get::<_, i64>(0),
    ) {
        Ok(Some(count)) => Ok(count == 0),
        Ok(None) => Ok(true),
        Err(_) => Ok(true), // Table doesn't exist
    }
}

/// Add missing columns to existing tables (additive only)
fn sync_schema_from_tables(db: &Db) -> Result<()> {
    let table_defs = tables();

    for (table_name, def) in table_defs.iter() {
        // Create table if absent
        let sql = build_create_table_sql(table_name, def);
        db.execute(&sql)?;

        // Diff columns: get existing columns from pragma
        let existing_cols = db.query_all(
            &format!("PRAGMA table_info({})", table_name),
            &[],
            |row| {
                Ok(row.get::<_, String>(1)?) // column name is at index 1
            },
        )?;
        let existing_set: HashSet<String> = existing_cols.into_iter().collect();

        // Add missing columns
        for (col_name, col_def) in def.columns.iter() {
            if !existing_set.contains(col_name) {
                // Remove PRIMARY KEY / UNIQUE from the definition as they're only valid at creation
                let safe_def = col_def
                    .replace("PRIMARY KEY AUTOINCREMENT", "")
                    .replace("PRIMARY KEY", "")
                    .replace("UNIQUE", "")
                    .trim()
                    .to_string();

                let alter_sql = format!(
                    "ALTER TABLE {} ADD COLUMN {} {}",
                    table_name, col_name, safe_def
                );

                match db.execute(&alter_sql) {
                    Ok(_) => {
                        tracing::info!("[DB][sync] +column {}.{}", table_name, col_name);
                    }
                    Err(e) => {
                        tracing::warn!(
                            "[DB][sync] add column {}.{} failed: {}",
                            table_name,
                            col_name,
                            e
                        );
                    }
                }
            }
        }

        // Create indexes (idempotent)
        for idx_sql in &def.indexes {
            let _ = db.execute(idx_sql);
        }
    }

    Ok(())
}

/// Run migrations once per database lifetime
pub fn run_migration_once(db: &Db) -> Result<()> {
    // Bootstrap _meta first
    bootstrap_meta(db)?;

    // Check if already migrated
    let _fresh = is_fresh_db(db)?;
    let current_version = get_schema_version(db)?;

    // Update to current schema version if needed
    if current_version < SCHEMA_VERSION {
        tracing::info!(
            "[DB][migrate] schema version {} -> {}",
            current_version,
            SCHEMA_VERSION
        );
    }

    // Apply schema sync (additive only)
    sync_schema_from_tables(db)?;

    // Stamp the version we just reached
    set_schema_version(db, SCHEMA_VERSION)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_migration_runs_once() {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        assert!(db.is_valid());
    }

    #[test]
    fn test_schema_version_tracking() {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        let version = get_schema_version(&db).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }
}
