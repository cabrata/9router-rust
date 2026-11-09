//! Read `mitmAlias` mappings for the intercept path.
//! Port of src/mitm/dbReader.js.
//!
//! The JS standalone MITM process reads a JSON cache (`$DATA_DIR/mitm/aliases.json`)
//! because it has no SQLite binding. Rust runs in-process, so we read the DB
//! directly — same source of truth, no sync step.

use serde_json::Value;

/// Aliases for one tool: `{ "<raw model>": "<target model>", ... }`.
///
/// Fail-open: a DB error yields `None` so the caller passes the request through
/// untouched instead of failing the IDE's request.
pub fn get_mitm_alias(tool: &str) -> Option<Value> {
    let db = nr_db::db().ok()?;
    let all = nr_db::get_mitm_alias(&db, Some(tool)).ok()?;
    // `get_mitm_alias` returns `{}` for a missing tool; treat that as absent.
    if all.as_object().is_some_and(|o| o.is_empty()) {
        return None;
    }
    Some(all)
}

/// Every tool's alias map.
pub fn get_all_mitm_aliases() -> Option<Value> {
    let db = nr_db::db().ok()?;
    nr_db::get_mitm_alias(&db, None).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_alias_is_none_not_error() {
        // No DB configured in a bare test process: must fail open, not panic.
        let v = get_mitm_alias("antigravity");
        assert!(v.is_none() || v.unwrap().is_object());
    }
}
