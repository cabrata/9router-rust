//! MITM filesystem locations. Port of src/mitm/paths.js.
//!
//! Folds into `nr_core::paths` (`$DATA_DIR`, else `~/.9router`) so the Rust
//! build opens the same state dir as the JS build.

use std::path::PathBuf;

/// `$DATA_DIR/mitm` — holds `rootCA.key`, `rootCA.crt`, `aliases.json`, PID/lock files.
pub fn mitm_dir() -> PathBuf {
    nr_core::paths::data_dir().join("mitm")
}

/// `$DATA_DIR/logs/mitm` — debug request/response dumps (dev only).
pub fn dump_dir() -> PathBuf {
    nr_core::paths::data_dir().join("logs").join("mitm")
}
