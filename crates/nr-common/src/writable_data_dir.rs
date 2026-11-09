//! Port of open-sse/utils/writableDataDir.js.
//!
//! Resolve a writable data directory: `DATA_DIR` first, then `~/.9router`, then the
//! temp dir. Some installs point `DATA_DIR` at root-owned paths (e.g. `/var/lib`) that
//! the server process cannot write browser profiles or runtime installs into.

use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

static CACHED: LazyLock<Mutex<Option<PathBuf>>> = LazyLock::new(|| Mutex::new(None));

/// `fs.accessSync(base, W_OK)` — the directory must already exist and accept a write.
fn is_writable(base: &Path) -> bool {
    // Write a probe file rather than inspecting mode bits: access(W_OK) is about what
    // the process may do, not what the mode string says (root writes read-only dirs).
    let probe = base.join(format!(".9router-write-check-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

fn candidate(base: Option<PathBuf>) -> Option<PathBuf> {
    let base = base?;
    if std::fs::create_dir_all(&base).is_err() {
        return None;
    }
    is_writable(&base).then_some(base)
}

/// `getWritableDataDir` — first writable candidate wins; falls back to the cwd.
/// The result is cached for the life of the process, as in JS.
pub fn get_writable_data_dir() -> PathBuf {
    if let Some(cached) = CACHED.lock().ok().and_then(|c| c.clone()) {
        return cached;
    }

    let configured = std::env::var_os("DATA_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let default = Some(nr_core::paths::default_dir());
    let tmp = Some(std::env::temp_dir().join("9router"));

    let resolved = candidate(configured)
        .or_else(|| candidate(default))
        .or_else(|| candidate(tmp))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    if let Ok(mut slot) = CACHED.lock() {
        *slot = Some(resolved.clone());
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writable_dir_is_created_and_usable() {
        let dir = get_writable_data_dir();
        assert!(dir.exists(), "{} should exist", dir.display());
        // Cached: a second call returns the same path.
        assert_eq!(get_writable_data_dir(), dir);
    }

    #[test]
    fn rejects_unwritable_path() {
        assert!(!is_writable(Path::new("/proc/definitely-not-a-dir")));
    }
}
