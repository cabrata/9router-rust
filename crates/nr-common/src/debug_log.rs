//! Port of open-sse/utils/debugLog.js.
//!
//! The JS build gates on `NODE_ENV !== "production"`. Rust has no such global, so the
//! equivalent gate is `cfg!(debug_assertions)` — enabled for debug builds only.

use std::sync::atomic::{AtomicBool, Ordering};

static DEV: AtomicBool = AtomicBool::new(cfg!(debug_assertions));

/// Override the dev gate (used by tests; mirrors toggling NODE_ENV).
pub fn set_dev(dev: bool) {
    DEV.store(dev, Ordering::Relaxed);
}

/// `isDebugEnabled` — true when dev logging is active.
pub fn is_dev_enabled() -> bool {
    DEV.load(Ordering::Relaxed)
}

/// `dbg(tag, msg)` — log `[DBG:tag] msg` at debug level, only in dev.
pub fn dbg(tag: &str, msg: &str) {
    if !is_dev_enabled() {
        return;
    }
    tracing::debug!("[DBG:{tag}] {msg}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn respects_gate() {
        set_dev(false);
        dbg("t", "suppressed");
        assert!(!is_dev_enabled());
        set_dev(true);
        assert!(is_dev_enabled());
        dbg("t", "emitted");
        set_dev(cfg!(debug_assertions));
    }
}
