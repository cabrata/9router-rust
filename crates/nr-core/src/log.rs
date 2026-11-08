//! Request logger handle — the JS `log` object threaded through chatCore
//! (src/sse/utils/logger.js). The engine owner ports the colored console
//! format; these method signatures are the contract.

use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Clone, Default)]
pub struct Logger {
    counter: Arc<AtomicU64>,
}

impl Logger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn debug(&self, tag: &str, msg: impl AsRef<str>) {
        tracing::debug!("[{tag}] {}", msg.as_ref());
    }

    pub fn info(&self, tag: &str, msg: impl AsRef<str>) {
        tracing::info!("[{tag}] {}", msg.as_ref());
    }

    pub fn warn(&self, tag: &str, msg: impl AsRef<str>) {
        tracing::warn!("[{tag}] {}", msg.as_ref());
    }

    pub fn error(&self, tag: &str, msg: impl AsRef<str>) {
        tracing::error!("[{tag}] {}", msg.as_ref());
    }

    /// One correlated request line (JS `log.line(reqTag, icon, text)`).
    pub fn line(&self, req_tag: &str, icon: &str, msg: impl AsRef<str>) {
        tracing::info!("{req_tag} {icon} {}", msg.as_ref());
    }

    /// JS `log.errorLine(reqTag, icon, text)`.
    pub fn error_line(&self, req_tag: &str, icon: &str, msg: impl AsRef<str>) {
        tracing::error!("{req_tag} {icon} {}", msg.as_ref());
    }

    /// Stable tag for one CLI conversation (JS `log.tagForSession(seed)`).
    pub fn tag_for_session(&self, seed: &str) -> String {
        if seed.is_empty() {
            return self.next_tag();
        }
        let mut h = std::collections::hash_map::DefaultHasher::new();
        seed.hash(&mut h);
        format!("[{:04x}]", h.finish() & 0xffff)
    }

    /// Fresh per-request tag (JS `log.nextTag()`).
    pub fn next_tag(&self) -> String {
        format!("[#{}]", self.counter.fetch_add(1, Ordering::Relaxed) + 1)
    }
}
