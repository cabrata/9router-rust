//! SQLite tier for `nr_common::thought_signature_store`.
//!
//! The JS original calls `makeKv("gemini_thought_signatures")` directly. Here
//! `nr-common` sits below `nr-db`, so the persistent tier is a pluggable
//! [`SignatureKv`] that [`install`] wires at startup (from `install_real_engines`).
//!
//! Every method swallows its errors: the JS kv calls are `.catch(() => {})` and
//! the whole store is fail-open.

use std::sync::Arc;

use nr_common::thought_signature_store::{scope, SignatureKv};
use nr_core::{Json, JsonMap};
use nr_db::helpers::{parse_json, stringify_json};
use nr_db::Db;
use serde_json::json;

/// `SignatureKv` over the `kv` table, scoped to `gemini_thought_signatures`.
pub struct DbSignatureKv {
    db: Db,
}

impl DbSignatureKv {
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

impl SignatureKv for DbSignatureKv {
    fn get(&self, key: &str) -> Option<Json> {
        let row = self
            .db
            .query_row(
                "SELECT value FROM kv WHERE scope = ? AND key = ?",
                &[&scope(), &key],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .flatten()?;
        Some(parse_json(Some(row), Json::Null))
    }

    fn set(&self, key: &str, value: Json) {
        let _ = self.db.execute_with_params(
            "INSERT INTO kv(scope, key, value) VALUES(?, ?, ?) \
             ON CONFLICT(scope, key) DO UPDATE SET value = excluded.value",
            (scope(), key, stringify_json(&value)),
        );
    }

    fn remove(&self, key: &str) {
        let _ = self.db.execute_with_params(
            "DELETE FROM kv WHERE scope = ? AND key = ?",
            (scope(), key),
        );
    }

    fn get_all(&self) -> JsonMap {
        let rows = self
            .db
            .query_all(
                "SELECT key, value FROM kv WHERE scope = ?",
                &[&scope()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .unwrap_or_default();

        let mut out = JsonMap::new();
        for (key, value) in rows {
            out.insert(key, parse_json(Some(value), json!({})));
        }
        out
    }
}

/// Install the SQLite tier into the shared signature store (call once at boot).
pub fn install(db: Db) {
    nr_common::thought_signature_store::install_persistence(Arc::new(DbSignatureKv::new(db)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use nr_common::thought_signature_store as store;

    /// One test per binary: the store's persistence slot is a process global, so
    /// two tests installing different databases would race.
    #[test]
    fn persisted_signatures_round_trip_through_sqlite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::open(dir.path().join("data.sqlite")).expect("open db");
        install(db.clone());

        let kv = DbSignatureKv::new(db.clone());
        kv.set("call_rt", json!({"signature": "sig-rt", "family": "gemini", "createdAt": 1}));
        assert_eq!(kv.get("call_rt").unwrap()["signature"], json!("sig-rt"));
        assert_eq!(kv.get_all().len(), 1);

        // The store reads it back on a RAM miss.
        assert_eq!(
            store::get("call_rt", None, Some("gemini-3-pro")),
            Some("sig-rt".to_string())
        );

        kv.remove("call_rt");
        assert!(kv.get("call_rt").is_none());
        assert!(kv.get_all().is_empty());

        // Store writes land in the same scope the JS build uses.
        store::store("call_w", "sig-w", None, Some("gemini-3-pro"));
        let count: i64 = db
            .query_row(
                "SELECT count(*) FROM kv WHERE scope = ?",
                &[&store::scope()],
                |row| row.get(0),
            )
            .unwrap()
            .unwrap_or(0);
        assert_eq!(count, 1);
        assert_eq!(store::get("call_w", None, Some("gemini-3-pro")), Some("sig-w".into()));
    }
}
