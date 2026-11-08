//! DeepSeek web executor utilities — port of deepseek-web/** and deepseek-web-done-terminator.js.
//! Includes PoW solver, stream format utilities, and drain scheduling.
//! TODO(dedupe): Implement PoW solver (DeepSeekHashV1 algorithm).

use serde_json::Value as Json;

/// PoW challenge from DeepSeek
#[derive(Clone, Debug)]
pub struct DeepSeekPowChallenge {
    pub algorithm: String,
    pub challenge: String,
    pub salt: String,
    pub difficulty: u64,
    pub expire_at: u64,
    pub signature: String,
    pub target_path: String,
}

/// Request a DeepSeek PoW challenge.
/// TODO(dedupe): Implement once browser/http layer is ready.
pub async fn request_deepseek_pow_challenge(
    _access_token: &str,
    _target_path: &str,
) -> Result<DeepSeekPowChallenge, Box<dyn std::error::Error>> {
    Err("PoW challenge request not yet implemented".into())
}

/// Solve DeepSeek PoW challenge synchronously.
/// shortcut: Uses placeholder until full algorithm port from open-sse/lib/deepseek-pow.js.
/// When to upgrade: After porting the full DeepSeekHashV1 solver.
pub fn solve_deepseek_pow(
    _challenge: &DeepSeekPowChallenge,
) -> Result<i64, Box<dyn std::error::Error>> {
    Err("PoW solver not yet implemented — deepseek-pow.js needs port".into())
}

/// Check if model is a thinking model (R1-like).
pub fn is_thinking_model(model: &str) -> bool {
    let lower = model.to_lowercase();
    lower.contains("think") || lower.contains("r1") || lower.contains("reason")
}

/// Check if model has web search capability.
pub fn is_search_model(model: &str) -> bool {
    let lower = model.to_lowercase();
    lower.contains("search") || lower.contains("fold")
}

/// Clean DeepSeek stream token markers from text.
pub fn clean_deepseek_token(text: &str) -> String {
    let s = text.replace("FINISHED", "");
    regex::Regex::new(r"^(SEARCH|WEB_SEARCH|SEARCHING)\s*")
        .ok()
        .and_then(|re| Some(re.replace(&s, "").to_string()))
        .unwrap_or(s)
}

/// Format stream content for output, handling search models and citations.
pub fn format_stream_content(raw: &str, model: &str) -> String {
    let text = clean_deepseek_token(raw);
    if !is_search_model(model) {
        return text;
    }
    if model.to_lowercase().contains("search-silent") {
        regex::Regex::new(r"\[citation:(\d+)\]")
            .ok()
            .map(|re| re.replace_all(&text, "").to_string())
            .unwrap_or(text)
    } else {
        regex::Regex::new(r"\[citation:(\d+)\]")
            .ok()
            .map(|re| re.replace_all(&text, "[$1]").to_string())
            .unwrap_or(text)
    }
}

/// Format search results as citations.
pub fn append_search_citations(search_results: &[Json], model: &str) -> String {
    if search_results.is_empty() || model.to_lowercase().contains("search-silent") {
        return String::new();
    }

    let mut citations: Vec<_> = search_results
        .iter()
        .filter_map(|r| {
            let cite_index = r.get("cite_index")?.as_u64()?;
            let title = r.get("title")?.as_str()?;
            let url = r.get("url")?.as_str()?;
            Some((cite_index, format!("[{}]: [{}]({})", cite_index, title, url)))
        })
        .collect();

    citations.sort_by_key(|(i, _)| *i);
    citations.into_iter().map(|(_, s)| s).collect::<Vec<_>>().join("\n")
}

/// Guard state for stream finish — ensures `finishOnce` is called at most once.
pub struct FinishOnceGuard {
    finished: std::sync::atomic::AtomicBool,
}

impl FinishOnceGuard {
    pub fn new() -> Self {
        FinishOnceGuard {
            finished: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn finish_once<F>(&self, f: F)
    where
        F: FnOnce(),
    {
        if !self.finished.swap(true, std::sync::atomic::Ordering::SeqCst) {
            // Ignore panics; controller may be closed.
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).ok();
        }
    }

    pub fn has_finished(&self) -> bool {
        self.finished.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Default for FinishOnceGuard {
    fn default() -> Self {
        Self::new()
    }
}

/// Drain scheduler for DeepSeek `response/status=FINISHED` → wait for trailing search_results
/// before closing the stream.
pub struct FinishedDrainScheduler {
    drain_timer: std::sync::Mutex<Option<std::time::Instant>>,
    drain_ms: u64,
}

impl FinishedDrainScheduler {
    pub fn new(drain_ms: u64) -> Self {
        FinishedDrainScheduler {
            drain_timer: std::sync::Mutex::new(None),
            drain_ms,
        }
    }

    pub fn schedule_finish_after_drain(&self) {
        let mut timer = self.drain_timer.lock().unwrap();
        *timer = Some(std::time::Instant::now());
    }

    pub fn clear_finished_drain(&self) {
        let mut timer = self.drain_timer.lock().unwrap();
        *timer = None;
    }

    pub fn is_drain_pending(&self) -> bool {
        if let Ok(timer) = self.drain_timer.lock() {
            if let Some(start) = *timer {
                start.elapsed().as_millis() < self.drain_ms as u128
            } else {
                false
            }
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_thinking_model() {
        assert!(is_thinking_model("deepseek-r1"));
        assert!(is_thinking_model("gpt-4-thinking"));
        assert!(!is_thinking_model("gpt-4"));
    }

    #[test]
    fn test_is_search_model() {
        assert!(is_search_model("deepseek-search"));
        assert!(is_search_model("grok-fold"));
        assert!(!is_search_model("gpt-4"));
    }

    #[test]
    fn test_clean_deepseek_token() {
        assert_eq!(clean_deepseek_token("FINISHED"), "");
        assert_eq!(clean_deepseek_token("hello FINISHED world"), "hello  world");
    }

    #[test]
    fn test_finish_once_guard() {
        let guard = FinishOnceGuard::new();
        assert!(!guard.has_finished());

        let mut count = 0;
        guard.finish_once(|| {
            count += 1;
        });
        guard.finish_once(|| {
            count += 1;
        });

        // Should only call once
        assert_eq!(count, 1);
        assert!(guard.has_finished());
    }
}
