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

/// Transaction context - allows executing queries within a transaction.
/// This holds a reference to the connection while the transaction is active.
/// The lock is held for the entire duration of the transaction.
pub struct DbTx<'a> {
    conn: &'a mut RusqliteConnection,
}

impl<'a> DbTx<'a> {
    /// Execute a single SQL statement (no parameters)
    pub fn execute(&self, sql: &str) -> Result<usize> {
        self.conn.execute(sql, [])
            .map_err(|e| DbError::from(e).into())
    }

    /// Execute SQL with parameters
    pub fn execute_with_params<P: rusqlite::Params>(&self, sql: &str, params: P) -> Result<usize> {
        self.conn.execute(sql, params)
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
        match self.conn.query_row(sql, params, mapper) {
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
        let mut stmt = self.conn.prepare(sql)
            .map_err(|e| DbError::from(e))?;
        let rows = stmt.query_map(params, mapper)
            .map_err(|e| DbError::from(e))?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row.map_err(|e| DbError::from(e))?);
        }
        Ok(result)
    }
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

    /// Execute a transaction with BEGIN IMMEDIATE, honoring busy_timeout.
    /// The closure receives a transaction context that can execute queries.
    /// On success, commits; on error, rolls back.
    ///
    /// The lock is held for the entire duration of the transaction,
    /// ensuring atomicity and preventing read-modify-write race conditions.
    pub fn with_tx<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&DbTx) -> Result<T>,
    {
        let mut conn = self.inner.lock();

        // Set busy_timeout to 5 seconds to handle SQLITE_BUSY
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| DbError::from(e))?;

        // Begin transaction with IMMEDIATE mode to acquire locks quickly
        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| DbError::TransactionFailed(e.to_string()))?;

        // Create transaction context for the closure
        let tx = DbTx { conn: &mut conn };

        // Run the closure
        match f(&tx) {
            Ok(result) => {
                // Commit on success
                conn.execute_batch("COMMIT")
                    .map_err(|e| DbError::TransactionFailed(e.to_string()))?;
                Ok(result)
            }
            Err(e) => {
                // Rollback on error
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Execute a transaction with automatic rollback on error (deprecated - use with_tx).
    /// This method is kept for compatibility but is less efficient than with_tx.
    pub fn transaction<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Db) -> Result<T>,
    {
        self.execute("BEGIN")?;
        match f(self) {
            Ok(val) => {
                self.execute("COMMIT")?;
                Ok(val)
            }
            Err(e) => {
                let _ = self.execute("ROLLBACK");
                Err(e)
            }
        }
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

    #[test]
    fn test_transaction_commit() {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();

        // Execute a transaction that commits
        db.with_tx(|tx| {
            tx.execute("INSERT INTO _meta(key, value) VALUES('tx_test', 'tx_value')")?;
            Ok(())
        }).unwrap();

        // Verify the insert was committed
        let result = db.query_row(
            "SELECT value FROM _meta WHERE key = 'tx_test'",
            &[],
            |row| row.get::<_, String>(0),
        ).unwrap();
        assert_eq!(result, Some("tx_value".to_string()));
    }

    #[test]
    fn test_transaction_rollback() {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();

        // Execute a transaction that fails
        let result: Result<()> = db.with_tx(|tx| {
            tx.execute("INSERT INTO _meta(key, value) VALUES('rollback_test', 'rollback_value')")?;
            Err(nr_core::Error::other("intentional error"))
        });
        assert!(result.is_err());

        // Verify the insert was rolled back
        let result = db.query_row(
            "SELECT value FROM _meta WHERE key = 'rollback_test'",
            &[],
            |row| row.get::<_, String>(0),
        ).unwrap();
        assert_eq!(result, None);
    }
}
