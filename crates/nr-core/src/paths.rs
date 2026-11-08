//! Data directory resolution. Port of src/lib/dataDir.js — it must match
//! exactly so the Rust build opens the same `~/.9router` (or `$DATA_DIR`)
//! state as the JS build.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

const APP_NAME: &str = "9router";

pub fn home_dir() -> PathBuf {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// `~/.9router` (Windows: `%APPDATA%\9router`), ignoring `DATA_DIR`.
/// Usage/log files of the JS build live here regardless of `DATA_DIR`.
pub fn default_dir() -> PathBuf {
    if cfg!(windows) {
        let appdata = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| home_dir().join("AppData").join("Roaming"));
        return appdata.join(APP_NAME);
    }
    home_dir().join(format!(".{APP_NAME}"))
}

fn resolve() -> PathBuf {
    let Some(configured) = std::env::var_os("DATA_DIR").filter(|v| !v.is_empty()) else {
        return default_dir();
    };
    let configured = PathBuf::from(configured);
    // A Linux-targeted .env on Windows: Unix absolute paths are not valid there.
    if cfg!(windows) && configured.to_string_lossy().starts_with('/') {
        tracing::warn!("[DATA_DIR] '{}' is a Unix path on Windows → fallback to default", configured.display());
        return default_dir();
    }
    match std::fs::create_dir_all(&configured) {
        Ok(()) => configured,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            tracing::warn!("[DATA_DIR] '{}' not writable → fallback ~/.{APP_NAME}", configured.display());
            default_dir()
        }
        Err(e) => panic!("[DATA_DIR] cannot create '{}': {e}", configured.display()),
    }
}

static DATA_DIR: LazyLock<PathBuf> = LazyLock::new(resolve);

/// `DATA_DIR` export of dataDir.js: `$DATA_DIR` or the default dir.
pub fn data_dir() -> &'static Path {
    &DATA_DIR
}
