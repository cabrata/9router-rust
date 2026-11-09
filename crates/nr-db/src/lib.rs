#![recursion_limit = "512"]

//! SQLite persistence, file-compatible with the JS build's
//! `${DATA_DIR}/db/data.sqlite`. Port of src/lib/db/** (schema, migrations,
//! repos, backup, legacy db.json import) plus src/lib/usageDb.js,
//! requestDetailsDb.js and disabledModelsDb.js. Owner: db agent (see AGENTS.md).

pub mod backup;
pub mod connection;
pub mod error;
pub mod helpers;
pub mod migrate;
pub mod repos;
pub mod schema;

use parking_lot::Mutex;
use std::sync::{Arc, OnceLock};

pub use connection::Db;
pub use repos::*;

// Process-wide default database handle
static DEFAULT_DB: OnceLock<Arc<Mutex<Option<Db>>>> = OnceLock::new();

/// Open or return the process-wide default database at `$DATA_DIR/db/data.sqlite`.
/// Uses lazy initialization on first call.
pub fn db() -> nr_core::Result<Db> {
    let handle = DEFAULT_DB.get_or_init(|| Arc::new(Mutex::new(None)));
    {
        let mut guard = handle.lock();
        if let Some(db) = guard.as_ref() {
            return Ok(db.clone());
        }
        let db = Db::open_default()?;
        *guard = Some(db.clone());
        Ok(db)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_db_initialization() {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        assert!(db.is_valid());
    }

    #[test]
    fn test_db_cloneable() {
        let dir = TempDir::new().unwrap();
        let db1 = Db::open_in(dir.path()).unwrap();
        let db2 = db1.clone();
        assert!(db2.is_valid());
    }
}
