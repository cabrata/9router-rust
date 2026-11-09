//! Port of `src/lib/consoleLogBuffer.js`.
//!
//! Captures process log lines into a bounded ring buffer and fans them out to
//! `/api/translator/console-logs/stream` subscribers. In Node the buffer hooks
//! `console.*`; here the subscribers are `tracing`, so [`capture`] is the entry
//! point a `tracing_subscriber` layer (or any caller) uses to feed a line in.
//!
//! Batches new lines on a 100 ms timer, exactly like the JS emitter, so a burst
//! of logs arrives as one SSE frame instead of one frame per line.

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::broadcast;

/// `CONSOLE_LOG_CONFIG.maxLines` from `src/shared/constants/config.js`.
const MAX_LINES: usize = 200;
/// `FLUSH_INTERVAL_MS`.
const FLUSH_INTERVAL_MS: u64 = 100;
/// `MAX_BATCH_LINES`.
const MAX_BATCH_LINES: usize = 50;
const CHANNEL_CAPACITY: usize = 256;

/// A message pushed to SSE subscribers.
#[derive(Clone, Debug)]
pub enum Broadcast {
    Lines(Vec<String>),
    Clear,
}

struct State {
    logs: Vec<String>,
    pending: Vec<String>,
    tx: broadcast::Sender<Broadcast>,
    flush_scheduled: bool,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| {
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        Mutex::new(State {
            logs: Vec::new(),
            pending: Vec::new(),
            tx,
            flush_scheduled: false,
        })
    })
}

/// Subscribe to new lines and clear notifications.
pub fn subscribe() -> broadcast::Receiver<Broadcast> {
    state().lock().unwrap().tx.subscribe()
}

/// All buffered lines, oldest first. `getConsoleLogs()`.
pub fn get_logs() -> Vec<String> {
    state().lock().unwrap().logs.clone()
}

/// Drop the buffer and tell subscribers to reset. `clearConsoleLogs()`.
pub fn clear_logs() {
    let mut st = state().lock().unwrap();
    st.logs.clear();
    st.pending.clear();
    let _ = st.tx.send(Broadcast::Clear);
}

/// Append one line. `appendLine()` + `scheduleFlush()`.
///
/// ANSI escapes are stripped the same way `formatArg`/`stripAnsi` do, so
/// terminal colours never reach the dashboard.
pub fn capture(level: &str, args: &[&str]) {
    let line = args
        .iter()
        .map(|a| strip_ansi(a))
        .collect::<Vec<_>>()
        .join(" ");
    capture_line(&format!("[{}] {line}", level.to_ascii_lowercase()).replace("[log] ", ""));
}

/// Append an already-formatted line, bypassing level tagging.
pub fn capture_line(line: &str) {
    let line = strip_ansi(line);
    let mut st = state().lock().unwrap();
    st.logs.push(line.clone());
    if st.logs.len() > MAX_LINES {
        let overflow = st.logs.len() - MAX_LINES;
        st.logs.drain(0..overflow);
    }
    st.pending.push(line);
    if st.pending.len() >= MAX_BATCH_LINES {
        let lines = std::mem::take(&mut st.pending);
        st.flush_scheduled = false;
        let _ = st.tx.send(Broadcast::Lines(lines));
        return;
    }
    drop(st);
    schedule_flush();
}

fn schedule_flush() {
    let first = {
        let mut st = state().lock().unwrap();
        if st.flush_scheduled {
            false
        } else {
            st.flush_scheduled = true;
            true
        }
    };
    if !first {
        return;
    }
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(FLUSH_INTERVAL_MS)).await;
        flush_pending();
    });
}

fn flush_pending() {
    let mut st = state().lock().unwrap();
    st.flush_scheduled = false;
    if st.pending.is_empty() {
        return;
    }
    let lines = std::mem::take(&mut st.pending);
    let _ = st.tx.send(Broadcast::Lines(lines));
}

/// `stripAnsi`: drop `\x1b[...m` SGR sequences.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            // Skip CSI: ESC '[' then digits/';' then a final byte 0x40..=0x7e.
            let mut j = i + 2;
            while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'm' {
                i = j + 1;
                continue;
            }
        }
        let ch = input[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// `/api/translator/console-logs` payload. `{ success: true, logs }`.
pub fn logs_payload() -> Value {
    json!({ "success": true, "logs": get_logs() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_sgr_sequences() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m plain"), "red plain");
        assert_eq!(strip_ansi("no codes"), "no codes");
        // A lone ESC without CSI is preserved.
        assert_eq!(strip_ansi("a\x1bb"), "a\x1bb");
    }

    #[test]
    fn ring_buffer_is_bounded() {
        clear_logs();
        for i in 0..(MAX_LINES + 25) {
            capture_line(&format!("line {i}"));
        }
        let logs = get_logs();
        assert_eq!(logs.len(), MAX_LINES);
        assert_eq!(logs.last().map(String::as_str), Some("line 224"));
    }

    #[test]
    fn clear_empties_buffer() {
        capture_line("x");
        assert!(!get_logs().is_empty());
        clear_logs();
        assert!(get_logs().is_empty());
    }

    #[tokio::test]
    async fn subscribers_receive_batches() {
        clear_logs();
        let mut rx = subscribe();
        capture_line("hello");
        let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("flush within 2s")
            .expect("broadcast recv");
        match msg {
            Broadcast::Lines(lines) => assert_eq!(lines, vec!["hello".to_string()]),
            Broadcast::Clear => panic!("expected lines, got clear"),
        }
    }

    #[tokio::test]
    async fn clear_notifies_subscribers() {
        let mut rx = subscribe();
        clear_logs();
        let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("clear within 2s")
            .expect("broadcast recv");
        assert!(matches!(msg, Broadcast::Clear));
    }
}
