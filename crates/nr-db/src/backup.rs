//! DB safety backups — taken ONLY before a schema change (see migrate).
//! Port of src/lib/db/backup.js.
//!
//! Backups are a best-effort safety net before schema migrations. There is NO
//! automated restore path; recovery is manual (copy a backup file back).
//! Backups intentionally EXCLUDE the `requestDetails` table (observability log,
//! auto-pruned, non-critical) so a multi-hundred-MB DB backs up as a few MB.
//! Only the newest KEEP_BACKUPS are kept; older ones are pruned automatically.

use crate::connection::Db;
use nr_core::Result;
use std::fs;
use std::path::{Path, PathBuf};

const KEEP_BACKUPS: usize = 3;

/// Tables excluded from safety backups (large, non-critical, reproducible).
const BACKUP_EXCLUDE_TABLES: &[&str] = &["requestDetails"];

/// `$DATA_DIR/db/backups` — sibling of the `data.sqlite` file we are backing up.
pub fn backups_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("db").join("backups")
}

/// `timestampSlug(date)`: local time `YYYYMMDD-HHMMSS`.
fn timestamp_slug() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()
}

/// Create a fresh backup directory named `<label>-<version>-<timestamp>`.
// shortcut: the JS uses the package.json app version; the Rust build has no
// package.json, so CARGO_PKG_VERSION is used. Backup dir names are informational
// only (nothing parses them), so this cannot affect restore compatibility.
pub fn make_backup_dir(data_dir: &Path, label: &str) -> Result<PathBuf> {
    let dir = backups_dir(data_dir);
    fs::create_dir_all(&dir)?;
    let slug = format!("{}-{}-{}", label, env!("CARGO_PKG_VERSION"), timestamp_slug());
    let full = dir.join(slug);
    fs::create_dir_all(&full)?;
    Ok(full)
}

/// Copy one file into a backup dir (no-op when the source is absent).
pub fn backup_file(src: &Path, dest_dir: &Path, dest_name: Option<&str>) -> Option<PathBuf> {
    if !src.exists() {
        return None;
    }
    let name = dest_name
        .map(str::to_string)
        .or_else(|| src.file_name().map(|n| n.to_string_lossy().to_string()))?;
    let dest = dest_dir.join(name);
    fs::copy(src, &dest).ok()?;
    Some(dest)
}

/// Lightweight DB backup via ATTACH: create an empty sqlite file, copy every
/// table EXCEPT the excluded ones into it. Avoids duplicating the huge
/// observability log, so the backup stays small regardless of DB size.
pub fn backup_db_lite(db: &Db, dest_dir: &Path, dest_name: &str) -> Result<PathBuf> {
    let dest = dest_dir.join(dest_name);
    let _ = fs::remove_file(&dest);

    // ATTACH takes a string literal: single quotes are doubled, nothing else.
    let escaped = dest.to_string_lossy().replace('\'', "''");
    db.execute(&format!("ATTACH DATABASE '{}' AS bak", escaped))?;

    let copy = || -> Result<()> {
        let tables: Vec<(String, String)> = db.query_all(
            "SELECT name, sql FROM main.sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            &[],
            |row| Ok((row.get(0)?, row.get::<_, Option<String>>(1)?.unwrap_or_default())),
        )?;

        db.with_tx(|tx| {
            for (name, sql) in tables {
                if BACKUP_EXCLUDE_TABLES.contains(&name.as_str()) || sql.is_empty() {
                    continue;
                }
                // Recreate table structure in backup DB, then copy rows.
                tx.execute(&sql.replacen("CREATE TABLE ", "CREATE TABLE bak.", 1))?;
                tx.execute(&format!("INSERT INTO bak.{} SELECT * FROM main.{}", name, name))?;
            }
            Ok(())
        })
    };

    let result = copy();
    let _ = db.execute("DETACH DATABASE bak");
    result?;
    Ok(dest)
}

/// Keep only the newest KEEP_BACKUPS backup directories.
pub fn prune_old_backups(data_dir: &Path) {
    let dir = backups_dir(data_dir);
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };

    let mut dirs: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| {
            let mtime = e.metadata().and_then(|m| m.modified()).ok()?;
            Some((mtime, e.path()))
        })
        .collect();

    dirs.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, old) in dirs.into_iter().skip(KEEP_BACKUPS) {
        let _ = fs::remove_dir_all(old);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn backup_dir_is_created_under_db_backups() {
        let dir = TempDir::new().unwrap();
        let made = make_backup_dir(dir.path(), "schema-0-to-3").unwrap();
        assert!(made.is_dir());
        assert!(made.starts_with(dir.path().join("db").join("backups")));
    }

    #[test]
    fn lite_backup_copies_tables_but_not_request_details() {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        db.execute("INSERT INTO settings(id, data) VALUES(1, '{\"a\":1}')").unwrap();
        db.execute("INSERT INTO requestDetails(id, timestamp, data) VALUES('r1', 't', '{}')").unwrap();

        let dest_dir = make_backup_dir(dir.path(), "schema-test").unwrap();
        let dest = backup_db_lite(&db, &dest_dir, "data.sqlite").unwrap();
        assert!(dest.is_file());

        let backup = Db::open(&dest).unwrap();
        let settings: Option<String> = backup
            .query_row("SELECT data FROM settings WHERE id = 1", &[], |r| r.get(0))
            .unwrap();
        assert_eq!(settings.as_deref(), Some("{\"a\":1}"));
        let details: Option<i64> = backup
            .query_row("SELECT COUNT(*) FROM requestDetails", &[], |r| r.get(0))
            .unwrap();
        assert_eq!(details, Some(0));
    }

    #[test]
    fn prune_keeps_newest_three() {
        let dir = TempDir::new().unwrap();
        let backups = backups_dir(dir.path());
        fs::create_dir_all(&backups).unwrap();
        for i in 0..6 {
            let p = backups.join(format!("b{i}"));
            fs::create_dir_all(&p).unwrap();
            // Stagger mtime deterministically: b0 oldest … b5 newest.
            let marker = p.join("m");
            fs::write(&marker, b"x").unwrap();
            let t = std::time::SystemTime::now() - std::time::Duration::from_secs(100 - i * 10);
            let f = fs::OpenOptions::new().write(true).open(&marker).unwrap();
            let _ = f.set_modified(t);
        }

        prune_old_backups(dir.path());

        let left: Vec<String> = fs::read_dir(&backups)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(left.len(), KEEP_BACKUPS);
    }
}
