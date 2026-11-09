//! DeepSeek web executor utilities — port of deepseek-web/** and deepseek-web-done-terminator.js.
//! Includes PoW solver (DeepSeekHashV1), stream format utilities, and drain scheduling.

use serde_json::{json, Value as Json};

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

// ── DeepSeekHashV1: Keccak-f[1600] with round 0 skipped ──────────────────────────────
//
// Ported from open-sse/lib/deepseek-pow-hash.js. The JS implementation uses a custom
// Keccak-f[1600] variant that executes only rounds 1-23 (skipping round 0).
// This is **not** standard SHA3-256 (which uses all 24 rounds).

const KECCAK_ROUNDS: usize = 23; // Skips round 0, runs rounds 1-23
const KECCAK_BLOCK: usize = 136; // Rate in bytes for SHA3-256

/// Keccak-f permutation constants (iota round constants).
/// Pre-computed for all 24 rounds (we'll use rounds 1-23).
fn keccak_iota_constants() -> ([u32; 24], [u32; 24]) {
    // High and low 32-bit words of the iota constants for rounds 0-23.
    // These match the values in the JS implementation.
    let iota_h = [
        0x00000001, 0x00000000, 0x00000000, 0x00000000,
        0x00000001, 0x00000001, 0x00000001, 0x00000004,
        0x00000004, 0x00000000, 0x00000001, 0x00000001,
        0x00000004, 0x00000000, 0x00000004, 0x00000001,
        0x00000005, 0x00000000, 0x00000001, 0x00000001,
        0x00000004, 0x00000004, 0x00000000, 0x00000001,
    ];
    let iota_l = [
        0x00000000, 0x00000089, 0x8000008b, 0x80008080,
        0x0000008b, 0x00008000, 0x80008081, 0x80000001,
        0x8000008b, 0x8000008b, 0x8000008b, 0x8000008b,
        0x8000008b, 0x00008000, 0x80008081, 0x8000008b,
        0x80000001, 0x80008080, 0x8000008b, 0x00000001,
        0x8000008b, 0x00008081, 0x80000001, 0x80008080,
    ];
    (iota_h, iota_l)
}

/// Keccak rotation offsets (pi permutation rotations).
fn keccak_rotation_offsets() -> [usize; 24] {
    [
        1, 3, 6, 10, 15, 21, 28, 36, 45, 55, 2, 14,
        27, 41, 56, 8, 25, 43, 62, 18, 39, 61, 20, 44,
    ]
}

/// Keccak pi permutation indices.
fn keccak_pi_indices() -> [usize; 24] {
    [
        10, 7, 11, 17, 18, 3, 5, 16, 8, 21, 24, 4,
        15, 23, 19, 13, 12, 2, 20, 14, 22, 9, 6, 1,
    ]
}

/// Rotate left a 64-bit value (represented as two 32-bit words).
#[inline]
fn rotl64(h: u32, l: u32, n: usize) -> (u32, u32) {
    if n == 0 {
        (h, l)
    } else if n < 32 {
        ((h << n) | (l >> (32 - n)), (l << n) | (h >> (32 - n)))
    } else {
        let n = n - 32;
        ((l << n) | (h >> (32 - n)), (h << n) | (l >> (32 - n)))
    }
}

/// Keccak-f[1600] permutation with custom round count (1-23, skipping round 0).
fn keccak_p(state: &mut [u32; 50], num_rounds: usize) {
    let (iota_h, iota_l) = keccak_iota_constants();
    let rotations = keccak_rotation_offsets();
    let pi_indices = keccak_pi_indices();

    for round in (24 - num_rounds)..24 {
        // Theta: column parity and mixing
        let mut bc = [0u32; 10];
        for x in 0..10 {
            bc[x] = state[x] ^ state[x + 10] ^ state[x + 20] ^ state[x + 30] ^ state[x + 40];
        }

        for x in 0..10 {
            let t_idx1 = (x + 8) % 10;
            let t_idx0 = (x + 2) % 10;
            let (t_h, t_l) = rotl64(bc[t_idx0], bc[t_idx0 + 1], 1);
            let th = t_h ^ bc[t_idx1];
            let tl = t_l ^ bc[t_idx1 + 1];

            for y in (0..50).step_by(10) {
                state[x + y] ^= th;
                state[x + y + 1] ^= tl;
            }
        }

        // Rho and Pi
        let mut cur_h = state[2];
        let mut cur_l = state[3];
        for t in 0..24 {
            let shift = rotations[t];
            let (th, tl) = rotl64(cur_h, cur_l, shift);
            let pi = pi_indices[t] * 2;
            cur_h = state[pi];
            cur_l = state[pi + 1];
            state[pi] = th;
            state[pi + 1] = tl;
        }

        // Chi
        for y in (0..50).step_by(10) {
            let b0 = state[y];
            let b1 = state[y + 1];
            let b2 = state[y + 2];
            let b3 = state[y + 3];
            state[y] ^= !state[y + 2] & state[y + 4];
            state[y + 1] ^= !state[y + 3] & state[y + 5];
            state[y + 2] ^= !state[y + 4] & state[y + 6];
            state[y + 3] ^= !state[y + 5] & state[y + 7];
            state[y + 4] ^= !state[y + 6] & state[y + 8];
            state[y + 5] ^= !state[y + 7] & state[y + 9];
            state[y + 6] ^= !state[y + 8] & b0;
            state[y + 7] ^= !state[y + 9] & b1;
            state[y + 8] ^= !b0 & b2;
            state[y + 9] ^= !b1 & b3;
        }

        // Iota
        state[0] ^= iota_h[round];
        state[1] ^= iota_l[round];
    }
}

/// DeepSeekHashV1 — find nonce such that hash(prefix + nonce) == target.
/// Uses Keccak-f[1600] with only rounds 1-23 (skipping round 0).
fn find_deepseek_pow_nonce(prefix: &str, challenge_hex: &str, difficulty: u64) -> i64 {
    // Parse target hash from hex string
    let mut target = [0u8; 32];
    for i in 0..32 {
        if i * 2 + 2 > challenge_hex.len() {
            return -1;
        }
        match u8::from_str_radix(&challenge_hex[i * 2..i * 2 + 2], 16) {
            Ok(byte) => target[i] = byte,
            Err(_) => return -1,
        }
    }

    let prefix_bytes = prefix.as_bytes();

    // Initialize state and absorb prefix
    let mut state = [0u32; 50];
    let mut state_bytes = [0u8; 200];
    let mut pos = 0;

    for &byte in prefix_bytes {
        state_bytes[pos] ^= byte;
        pos += 1;
        if pos == KECCAK_BLOCK {
            // Convert state_bytes to u32 array and permute
            for i in 0..50 {
                state[i] = u32::from_le_bytes([
                    state_bytes[i * 4],
                    state_bytes[i * 4 + 1],
                    state_bytes[i * 4 + 2],
                    state_bytes[i * 4 + 3],
                ]);
            }
            keccak_p(&mut state, KECCAK_ROUNDS);
            // Convert back to bytes
            for i in 0..50 {
                let bytes = state[i].to_le_bytes();
                state_bytes[i * 4] = bytes[0];
                state_bytes[i * 4 + 1] = bytes[1];
                state_bytes[i * 4 + 2] = bytes[2];
                state_bytes[i * 4 + 3] = bytes[3];
            }
            pos = 0;
        }
    }

    // Snapshot state after absorbing prefix
    let snapshot_bytes = state_bytes.clone();
    let snapshot_pos = pos;

    // Try nonces from 0 to difficulty
    for nonce in 0..difficulty {
        let mut bytes = snapshot_bytes.clone();
        let mut p = snapshot_pos;

        let nonce_str = nonce.to_string();
        for &byte in nonce_str.as_bytes() {
            bytes[p] ^= byte;
            p += 1;
            if p == KECCAK_BLOCK {
                // Permute
                for i in 0..50 {
                    state[i] = u32::from_le_bytes([
                        bytes[i * 4],
                        bytes[i * 4 + 1],
                        bytes[i * 4 + 2],
                        bytes[i * 4 + 3],
                    ]);
                }
                keccak_p(&mut state, KECCAK_ROUNDS);
                for i in 0..50 {
                    let w = state[i];
                    bytes[i * 4] = (w & 0xFF) as u8;
                    bytes[i * 4 + 1] = ((w >> 8) & 0xFF) as u8;
                    bytes[i * 4 + 2] = ((w >> 16) & 0xFF) as u8;
                    bytes[i * 4 + 3] = ((w >> 24) & 0xFF) as u8;
                }
                p = 0;
            }
        }

        // Finish: add padding
        bytes[p] ^= 0x06;
        bytes[KECCAK_BLOCK - 1] ^= 0x80;

        // Final permutation
        for i in 0..50 {
            state[i] = u32::from_le_bytes([
                bytes[i * 4],
                bytes[i * 4 + 1],
                bytes[i * 4 + 2],
                bytes[i * 4 + 3],
            ]);
        }
        keccak_p(&mut state, KECCAK_ROUNDS);

        // Extract first 32 bytes
        for i in 0..50 {
            let w = state[i];
            bytes[i * 4] = (w & 0xFF) as u8;
            bytes[i * 4 + 1] = ((w >> 8) & 0xFF) as u8;
            bytes[i * 4 + 2] = ((w >> 16) & 0xFF) as u8;
            bytes[i * 4 + 3] = ((w >> 24) & 0xFF) as u8;
        }

        // Compare with target
        if bytes[..32] == target {
            return nonce as i64;
        }
    }

    -1
}

/// Solve DeepSeek PoW challenge synchronously.
pub fn solve_deepseek_pow(challenge: &DeepSeekPowChallenge) -> Result<i64, Box<dyn std::error::Error>> {
    if challenge.algorithm != "DeepSeekHashV1" {
        return Err(format!("Unsupported algorithm: {}", challenge.algorithm).into());
    }

    let prefix = format!("{}_{}_", challenge.salt, challenge.expire_at);
    let nonce = find_deepseek_pow_nonce(&prefix, &challenge.challenge, challenge.difficulty);

    if nonce < 0 {
        Err("PoW solver failed to find nonce".into())
    } else {
        Ok(nonce)
    }
}

/// Check if model is a thinking model (R1-like).
pub fn is_thinking_model(model: &str) -> bool {
    let lower = model.to_lowercase();
    lower.contains("think") || lower.contains("r1") || lower.contains("reason")
}

/// Maximum difficulty for PoW computation.
pub const MAX_DEEPSEEK_POW_DIFFICULTY: u64 = 250_000;

/// Check if model has web search capability.
pub fn is_search_model(model: &str) -> bool {
    let lower = model.to_lowercase();
    lower.contains("search") || lower.contains("fold")
}

/// Clean DeepSeek stream token markers from text.
pub fn clean_deepseek_token(text: &str) -> String {
    let s = text.replace("FINISHED", "");
    // Order longer patterns first to avoid SEARCH matching prefix of SEARCHING
    regex::Regex::new(r"(?i)^(WEB_SEARCH|SEARCHING|SEARCH)\s*")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_thinking_model() {
        assert!(is_thinking_model("deepseek-r1"));
        assert!(is_thinking_model("DeepSeek-Think"));
        assert!(is_thinking_model("model-reasoning"));
        assert!(!is_thinking_model("deepseek-chat"));
    }

    #[test]
    fn test_is_search_model() {
        assert!(is_search_model("deepseek-search"));
        assert!(is_search_model("model-fold"));
        assert!(!is_search_model("deepseek-chat"));
    }

    #[test]
    fn test_clean_deepseek_token() {
        assert_eq!(clean_deepseek_token("FINISHED"), "");
        assert_eq!(clean_deepseek_token("WEB_SEARCH hello"), "hello");
        assert_eq!(clean_deepseek_token("SEARCHING world"), "world");
        assert_eq!(clean_deepseek_token("hello"), "hello");
    }

    #[test]
    fn test_format_stream_content_non_search() {
        let content = "hello world";
        let formatted = format_stream_content(content, "deepseek-chat");
        assert_eq!(formatted, "hello world");
    }

    #[test]
    fn test_format_stream_content_search() {
        let content = "[citation:1] hello [citation:2] world";
        let formatted = format_stream_content(content, "deepseek-search");
        assert_eq!(formatted, "[1] hello [2] world");
    }

    #[test]
    fn test_format_stream_content_search_silent() {
        let content = "[citation:1] hello [citation:2] world";
        let formatted = format_stream_content(content, "deepseek-search-silent");
        assert_eq!(formatted, " hello  world");
    }

    #[test]
    fn test_append_search_citations() {
        let results = vec![
            json!({"cite_index": 1, "title": "Page 1", "url": "http://example.com/1"}),
            json!({"cite_index": 2, "title": "Page 2", "url": "http://example.com/2"}),
        ];
        let citations = append_search_citations(&results, "deepseek-search");
        assert!(citations.contains("[1]: [Page 1](http://example.com/1)"));
        assert!(citations.contains("[2]: [Page 2](http://example.com/2)"));
    }

    #[test]
    fn test_append_search_citations_empty() {
        let results: Vec<Json> = vec![];
        let citations = append_search_citations(&results, "deepseek-search");
        assert_eq!(citations, "");
    }

    #[test]
    fn test_append_search_citations_silent() {
        let results = vec![json!({"cite_index": 1, "title": "Page", "url": "http://example.com"})];
        let citations = append_search_citations(&results, "deepseek-search-silent");
        assert_eq!(citations, "");
    }

    #[test]
    fn test_solve_deepseek_pow_invalid_algorithm() {
        let challenge = DeepSeekPowChallenge {
            algorithm: "InvalidAlgo".to_string(),
            challenge: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            salt: "salt".to_string(),
            difficulty: 10,
            expire_at: 1000,
            signature: "sig".to_string(),
            target_path: "/path".to_string(),
        };
        assert!(solve_deepseek_pow(&challenge).is_err());
    }

    #[test]
    fn test_solve_deepseek_pow_invalid_hex() {
        let challenge = DeepSeekPowChallenge {
            algorithm: "DeepSeekHashV1".to_string(),
            challenge: "ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ".to_string(),
            salt: "salt".to_string(),
            difficulty: 10,
            expire_at: 1000,
            signature: "sig".to_string(),
            target_path: "/path".to_string(),
        };
        assert!(solve_deepseek_pow(&challenge).is_err());
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
        assert!(is_thinking_model("claude-opus-thinking"));
        assert!(is_thinking_model("DEEPSEEK-REASON"));
        assert!(!is_thinking_model("gpt-4"));
        assert!(!is_thinking_model("deepseek-chat"));
    }

    #[test]
    fn test_is_search_model() {
        assert!(is_search_model("deepseek-search"));
        assert!(is_search_model("grok-fold"));
        assert!(is_search_model("DEEPSEEK-SEARCH"));
        assert!(!is_search_model("gpt-4"));
        assert!(!is_search_model("deepseek-chat"));
    }

    #[test]
    fn test_clean_deepseek_token() {
        assert_eq!(clean_deepseek_token("FINISHED"), "");
        assert_eq!(clean_deepseek_token("hello FINISHED world"), "hello  world");
        assert_eq!(clean_deepseek_token("SEARCH result"), "result");
        assert_eq!(clean_deepseek_token("WEB_SEARCH data"), "data");
        // \s* matches zero or more spaces, so "SEARCHING..." removes "SEARCHING" leaving "..."
        assert_eq!(clean_deepseek_token("SEARCHING..."), "...");
        assert_eq!(clean_deepseek_token("SEARCHING "), "");
        assert_eq!(clean_deepseek_token("search result"), "result"); // case-insensitive
        assert_eq!(clean_deepseek_token("hello"), "hello");
    }

    #[test]
    fn test_format_stream_content_non_search() {
        let content = "FINISHED hello world";
        // clean_deepseek_token removes "FINISHED" but leaves the space
        assert_eq!(format_stream_content(content, "deepseek-chat"), " hello world");
    }

    #[test]
    fn test_format_stream_content_search_with_citations() {
        let content = "result [citation:1] and [citation:2]";
        let formatted = format_stream_content(content, "deepseek-search");
        assert_eq!(formatted, "result [1] and [2]");
    }

    #[test]
    fn test_format_stream_content_search_silent() {
        let content = "result [citation:1] and [citation:2]";
        let formatted = format_stream_content(content, "deepseek-search-silent");
        assert_eq!(formatted, "result  and ");
    }

    #[test]
    fn test_append_search_citations() {
        let results = vec![
            serde_json::json!({
                "cite_index": 1,
                "title": "Example",
                "url": "https://example.com"
            }),
            serde_json::json!({
                "cite_index": 2,
                "title": "Test",
                "url": "https://test.com"
            }),
        ];
        let citations = append_search_citations(&results, "deepseek-search");
        assert!(citations.contains("[1]: [Example](https://example.com)"));
        assert!(citations.contains("[2]: [Test](https://test.com)"));
    }

    #[test]
    fn test_append_search_citations_silent() {
        let results = vec![
            serde_json::json!({
                "cite_index": 1,
                "title": "Example",
                "url": "https://example.com"
            }),
        ];
        let citations = append_search_citations(&results, "deepseek-search-silent");
        assert_eq!(citations, "");
    }

    #[test]
    fn test_append_search_citations_empty() {
        let results: Vec<_> = vec![];
        let citations = append_search_citations(&results, "deepseek-search");
        assert_eq!(citations, "");
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

    #[test]
    fn test_finish_once_guard_default() {
        let guard = FinishOnceGuard::default();
        assert!(!guard.has_finished());
        guard.finish_once(|| {});
        assert!(guard.has_finished());
    }

    #[test]
    fn test_finished_drain_scheduler() {
        let scheduler = FinishedDrainScheduler::new(50);
        assert!(!scheduler.is_drain_pending());

        scheduler.schedule_finish_after_drain();
        assert!(scheduler.is_drain_pending());

        scheduler.clear_finished_drain();
        assert!(!scheduler.is_drain_pending());
    }
}
