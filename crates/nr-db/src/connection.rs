use crate::error::DbError;
use crate::migrate::run_migration_once;
use crate::schema::{build_create_table_sql, tables, PRAGMA_SQL};
use nr_core::Result;
use rusqlite::Connection as RusqliteConnection;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A cloneable, thread-safe database handle
#[derive(Clone)]
pub struct Db {
    inner: Arc<parking_lot::Mutex<RusqliteConnection>>,
    path: PathBuf,
}

impl Db {
    /// Open a database at the given path, initializing schema and running migrations
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = RusqliteConnection::open(&path)
            .map_err(|e| DbError::from(e))?;

        // Note: conn is immutable because execute_batch takes &self
        conn.execute_batch(PRAGMA_SQL)
            .map_err(|e| DbError::from(e))?;

        let db = Db {
            inner: Arc::new(parking_lot::Mutex::new(conn)),
            path,
        };

        // Run migrations and schema sync
        run_migration_once(&db)?;

        Ok(db)
    }

    /// Open database at `$DATA_DIR/db/data.sqlite`
    pub fn open_default() -> Result<Self> {
        let data_dir = nr_core::paths::data_dir();
        let db_path = data_dir.join("db/data.sqlite");
        Self::open(db_path)
    }

    /// Open database in a specific data directory
    pub fn open_in(data_dir: impl AsRef<Path>) -> Result<Self> {
        let db_path = data_dir.as_ref().join("db/data.sqlite");
        Self::open(db_path)
    }

    /// Check if the database connection is valid
    pub fn is_valid(&self) -> bool {
        let conn = self.inner.lock();
        let result = conn.prepare("SELECT 1");
        result.is_ok()
    }

    /// Get the database file path
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Execute a single SQL statement (no parameters)
    pub fn execute(&self, sql: &str) -> Result<usize> {
        let conn = self.inner.lock();
        conn.execute(sql, [])
            .map_err(|e| DbError::from(e).into())
    }

    /// Execute SQL with parameters
    pub fn execute_with_params<P: rusqlite::Params>(&self, sql: &str, params: P) -> Result<usize> {
        let conn = self.inner.lock();
        conn.execute(sql, params)
            .map_err(|e| DbError::from(e).into())
    }

    /// Query a single row
    pub fn query_row<T, F>(
        &self,
        sql: &str,
        params: &[&dyn rusqlite::ToSql],
        mapper: F,
    ) -> Result<Option<T>>
    where
        F: FnOnce(&rusqlite::Row) -> rusqlite::Result<T>,
    {
        let conn = self.inner.lock();
        match conn.query_row(sql, params, mapper) {
            Ok(val) => Ok(Some(val)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(DbError::from(e).into()),
        }
    }

    /// Query multiple rows
    pub fn query_all<T, F>(
        &self,
        sql: &str,
        params: &[&dyn rusqlite::ToSql],
        mapper: F,
    ) -> Result<Vec<T>>
    where
        F: Fn(&rusqlite::Row) -> rusqlite::Result<T>,
    {
        let conn = self.inner.lock();
        let mut stmt = conn.prepare(sql)
            .map_err(|e| DbError::from(e))?;
        let rows = stmt.query_map(params, mapper)
            .map_err(|e| DbError::from(e))?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row.map_err(|e| DbError::from(e))?);
        }
        Ok(result)
    }

    /// Execute a transaction (simplified - not fully functional for now)
    pub fn transaction<F, T>(&self, _f: F) -> Result<T>
    where
        F: FnOnce(&Db) -> Result<T>,
    {
        // TODO(dedupe): Implement proper transaction support
        // For now, just fail with a clear message
        Err(nr_core::Error::other("Transactions not yet implemented in Rust port"))
    }

    /// Insert or update a single row, return last insert rowid
    pub fn insert_or_update(&self, sql: &str, params: &[&dyn rusqlite::ToSql]) -> Result<i64> {
        let conn = self.inner.lock();
        conn.execute(sql, params)
            .map_err(|e| DbError::from(e))?;
        Ok(conn.last_insert_rowid())
    }

    /// Initialize database schema
    pub fn init_schema(&self) -> Result<()> {
        let tables = tables();
        for (name, def) in tables.iter() {
            let sql = build_create_table_sql(name, def);
            self.execute(&sql)?;

            // Create indexes
            for idx in &def.indexes {
                self.execute(idx)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_open_and_close() {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        assert!(db.is_valid());
    }

    #[test]
    fn test_execute() {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        let rows = db.execute("INSERT INTO _meta(key, value) VALUES('test', 'value')")
            .unwrap();
        assert_eq!(rows, 1);
    }
}
