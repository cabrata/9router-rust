//! Port of open-sse/services/browserProfileCache.js
//!
//! Cache pruning for shared browser profiles.
//!
//! A persistent Chrome profile keeps every JS bundle, asset and GPU shader it
//! ever fetched. On the profiles used for web-cookie logins that dwarfs the
//! data we actually need: `default` grew to 334 MB, of which ~20 MB was
//! cookies + Local Storage + IndexedDB and the rest was Cache / Code Cache /
//! Service Worker. Pruning after the browser closes reclaims that space without
//! touching the session — login survives, only the warm cache is lost.

use std::fs;
use std::path::{Path, PathBuf};

/// Directories Chrome can rebuild on its own. Everything holding session state
/// (Cookies, Local Storage, IndexedDB, Login Data, Preferences) is deliberately
/// absent from this list.
const CACHE_DIRS: &[&str] = &[
    "Default/Cache",
    "Default/Code Cache",
    "Default/GPUCache",
    "Default/DawnWebGPUCache",
    "Default/DawnGraphiteCache",
    "Default/GrShaderCache",
    "Default/ShaderCache",
    "Default/Service Worker/CacheStorage",
    "Default/Service Worker/ScriptCache",
    "GraphiteDawnCache",
];

fn directory_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    for entry in entries.flatten() {
        let full = entry.path();
        // A file removed mid-walk is fine — the next prune catches it.
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => total += directory_size(&full),
            _ => {
                if let Ok(meta) = fs::metadata(&full) {
                    total += meta.len();
                }
            }
        }
    }
    total
}

#[derive(Debug, Default, Clone)]
pub struct PruneResult {
    pub freed_bytes: u64,
    pub removed: Vec<String>,
}

/// Delete the rebuildable caches inside one profile directory.
/// Safe only while the browser is closed; callers prune from their `finally`.
pub fn prune_profile_caches(profile_dir: &Path) -> PruneResult {
    let mut removed = Vec::new();
    let mut freed_bytes = 0u64;
    for relative in CACHE_DIRS {
        let target: PathBuf = profile_dir.join(relative);
        if !target.exists() {
            continue;
        }
        let before = directory_size(&target);
        // Locked file (antivirus, another Chrome) — leave it for next time.
        match fs::remove_dir_all(&target) {
            Ok(()) => {
                removed.push(relative.to_string());
                freed_bytes += before;
            }
            Err(_) => {}
        }
    }
    PruneResult { freed_bytes, removed }
}

/// Human-readable size for logs.
pub fn format_bytes(bytes: u64) -> String {
    if bytes == 0 {
        return "0 B".into();
    }
    let units = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 10.0 || unit == 0 {
        format!("{:.0} {}", value, units[unit])
    } else {
        format!("{:.1} {}", value, units[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(5), "5 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
        // Above GB stays GB
        assert_eq!(format_bytes(2048 * 1024 * 1024 * 1024), "2048 GB");
    }

    #[test]
    fn test_prune_profile_caches_removes_only_cache_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path();
        fs::create_dir_all(profile.join("Default/Cache")).unwrap();
        fs::write(profile.join("Default/Cache/blob"), vec![0u8; 1024]).unwrap();
        fs::create_dir_all(profile.join("Default/Local Storage")).unwrap();
        fs::write(profile.join("Default/Local Storage/leveldb"), b"session").unwrap();
        fs::write(profile.join("Cookies"), b"session-cookie").unwrap();

        let result = prune_profile_caches(profile);
        assert_eq!(result.removed, vec!["Default/Cache"]);
        assert_eq!(result.freed_bytes, 1024);

        // Cache gone, session state intact.
        assert!(!profile.join("Default/Cache").exists());
        assert!(profile.join("Default/Local Storage").exists());
        assert!(profile.join("Cookies").exists());
    }

    #[test]
    fn test_prune_missing_profile_is_noop() {
        let result = prune_profile_caches(Path::new("/nonexistent/profile"));
        assert_eq!(result.freed_bytes, 0);
        assert!(result.removed.is_empty());
    }

    #[test]
    fn test_directory_size_missing_is_zero() {
        assert_eq!(directory_size(Path::new("/nonexistent/dir")), 0);
    }
}
