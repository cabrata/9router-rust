//! DeepSeek web executor utilities — port of deepseek-web/** and deepseek-web-done-terminator.js.
//! Includes PoW solver (DeepSeekHashV1), stream format utilities, and drain scheduling.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Instant;
use nr_core::http::{proxy_fetch, FetchRequest, HeaderList, ProxyOptions};
use nr_core::{Error, Json, Result};
use tokio_util::sync::CancellationToken;

/// PoW challenge from DeepSeek
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct DeepSeekPowChallenge {
    pub algorithm: String,
    pub challenge: String,
    pub salt: String,
    pub difficulty: u64,
    pub expire_at: u64,
    pub signature: String,
    pub target_path: String,
}

/// Request a DeepSeek PoW challenge from `/api/v0/chat/create_pow_challenge`.
pub async fn request_deepseek_pow_challenge(
    access_token: &str,
    headers: &HeaderList,
    proxy: Option<&ProxyOptions>,
    cancel: Option<&CancellationToken>,
    target_path: &str,
) -> Result<DeepSeekPowChallenge> {
    let mut req_headers = headers.clone();
    req_headers.insert("Content-Type".into(), "application/json".into());
    req_headers.insert("Authorization".into(), format!("Bearer {}", access_token));

    let body = serde_json::json!({
        "target_path": target_path,
    });

    let fetch_req = FetchRequest {
        header_timeout: None,
        cancel: cancel.cloned(),
        direct_fetch: None,
        ..FetchRequest::post_json(
            "https://chat.deepseek.com/api/v0/chat/create_pow_challenge",
            req_headers,
            &body,
        )
    };

    let response = proxy_fetch(fetch_req, proxy).await?;
    if !response.ok() {
        return Err(Error::other(format!(
            "create_pow_challenge HTTP {}",
            response.status
        )));
    }

    let json_val = response.json_body().await?;
    let challenge_val = json_val
        .get("data")
        .and_then(|d| d.get("biz_data"))
        .and_then(|b| b.get("challenge"))
        .or_else(|| json_val.get("biz_data").and_then(|b| b.get("challenge")))
        .ok_or_else(|| Error::other("DeepSeek response missing PoW challenge"))?;

    let algorithm = challenge_val.get("algorithm").and_then(|v| v.as_str()).ok_or_else(|| Error::other("PoW challenge missing algorithm"))?;
    let challenge = challenge_val.get("challenge").and_then(|v| v.as_str()).ok_or_else(|| Error::other("PoW challenge missing challenge"))?;
    let salt = challenge_val.get("salt").and_then(|v| v.as_str()).ok_or_else(|| Error::other("PoW challenge missing salt"))?;
    let signature = challenge_val.get("signature").and_then(|v| v.as_str()).ok_or_else(|| Error::other("PoW challenge missing signature"))?;
    let difficulty = challenge_val.get("difficulty").and_then(|v| v.as_u64()).ok_or_else(|| Error::other("PoW challenge missing difficulty"))?;
    let expire_at = challenge_val.get("expire_at").and_then(|v| v.as_u64()).ok_or_else(|| Error::other("PoW challenge missing expire_at"))?;

    Ok(DeepSeekPowChallenge {
        algorithm: algorithm.to_string(),
        challenge: challenge.to_string(),
        salt: salt.to_string(),
        difficulty,
        expire_at,
        signature: signature.to_string(),
        target_path: target_path.to_string(),
    })
}

/// Format the solved PoW challenge as base64 JSON string for the `X-Ds-Pow-Response` header.
pub fn format_pow_response(challenge: &DeepSeekPowChallenge, answer: i64) -> String {
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine;

    let payload = serde_json::json!({
        "algorithm": challenge.algorithm,
        "challenge": challenge.challenge,
        "salt": challenge.salt,
        "answer": answer,
        "signature": challenge.signature,
        "target_path": challenge.target_path,
    });
    BASE64.encode(payload.to_string().as_bytes())
}

// ── DeepSeekHashV1: Keccak-f[1600] with round 0 skipped ──────────────────────────────
//
// Ported from open-sse/lib/deepseek-pow-hash.js.
// Runs rounds 1..=23 of Keccak-f[1600] (skipping round 0). Rate is 136 bytes (SHA3-256).

pub const MAX_DEEPSEEK_POW_DIFFICULTY: u64 = 250_000;
const KECCAK_ROUNDS: usize = 23;
const KECCAK_BLOCK: usize = 136;

const ROUND_CONSTANTS: [u64; 24] = [
    0x0000000000000001, 0x0000000000008082, 0x800000000000808a, 0x8000000080008000,
    0x000000000000808b, 0x0000000080000001, 0x8000000080008081, 0x8000000000008009,
    0x000000000000008a, 0x0000000000000088, 0x0000000080008009, 0x000000008000000a,
    0x000000008000808b, 0x800000000000008b, 0x8000000000008089, 0x8000000000008003,
    0x8000000000008002, 0x8000000000000080, 0x000000000000800a, 0x800000008000000a,
    0x8000000080008081, 0x8000000000008080, 0x0000000080000001, 0x8000000080008008,
];

const ROTATION_OFFSETS: [u32; 24] = [
    1, 3, 6, 10, 15, 21, 28, 36, 45, 55, 2, 14,
    27, 41, 56, 8, 25, 43, 62, 18, 39, 61, 20, 44,
];

const PI_INDICES: [usize; 24] = [
    10, 7, 11, 17, 18, 3, 5, 16, 8, 21, 24, 4,
    15, 23, 19, 13, 12, 2, 20, 14, 22, 9, 6, 1,
];

/// Keccak-f[1600] running only the last `rounds` rounds (for 23, rounds 1..24).
fn keccak_p(s: &mut [u64; 25], rounds: usize) {
    let start_round = 24 - rounds;
    for round in start_round..24 {
        // Theta
        let mut bc = [0u64; 5];
        for x in 0..5 {
            bc[x] = s[x] ^ s[x + 5] ^ s[x + 10] ^ s[x + 15] ^ s[x + 20];
        }
        for x in 0..5 {
            let t = bc[(x + 4) % 5] ^ bc[(x + 1) % 5].rotate_left(1);
            for y in 0..5 {
                s[y * 5 + x] ^= t;
            }
        }

        // Rho and Pi
        let mut cur = s[1];
        for t in 0..24 {
            let pi = PI_INDICES[t];
            let shift = ROTATION_OFFSETS[t];
            let next = s[pi];
            s[pi] = cur.rotate_left(shift);
            cur = next;
        }

        // Chi
        for y in 0..5 {
            let base = y * 5;
            let b0 = s[base];
            let b1 = s[base + 1];
            let b2 = s[base + 2];
            let b3 = s[base + 3];
            let b4 = s[base + 4];
            s[base] ^= !b1 & b2;
            s[base + 1] ^= !b2 & b3;
            s[base + 2] ^= !b3 & b4;
            s[base + 3] ^= !b4 & b0;
            s[base + 4] ^= !b0 & b1;
        }

        // Iota
        s[0] ^= ROUND_CONSTANTS[round];
    }
}

/// Convert byte buffer (200 bytes) into 25 u64 lanes (little endian), run permutation, convert back.
fn permute_state_bytes(state_bytes: &mut [u8; 200]) {
    let mut lanes = [0u64; 25];
    for i in 0..25 {
        lanes[i] = u64::from_le_bytes([
            state_bytes[i * 8],
            state_bytes[i * 8 + 1],
            state_bytes[i * 8 + 2],
            state_bytes[i * 8 + 3],
            state_bytes[i * 8 + 4],
            state_bytes[i * 8 + 5],
            state_bytes[i * 8 + 6],
            state_bytes[i * 8 + 7],
        ]);
    }
    keccak_p(&mut lanes, KECCAK_ROUNDS);
    for i in 0..25 {
        let b = lanes[i].to_le_bytes();
        state_bytes[i * 8..i * 8 + 8].copy_from_slice(&b);
    }
}

pub fn find_deepseek_pow_nonce(prefix: &str, challenge_hex: &str, difficulty: u64) -> i64 {
    if challenge_hex.len() != 64 {
        return -1;
    }
    let mut target = [0u8; 32];
    for i in 0..32 {
        match u8::from_str_radix(&challenge_hex[i * 2..i * 2 + 2], 16) {
            Ok(b) => target[i] = b,
            Err(_) => return -1,
        }
    }

    let prefix_bytes = prefix.as_bytes();
    let mut state_bytes = [0u8; 200];
    let mut pos = 0;

    for &b in prefix_bytes {
        state_bytes[pos] ^= b;
        pos += 1;
        if pos == KECCAK_BLOCK {
            permute_state_bytes(&mut state_bytes);
            pos = 0;
        }
    }

    let snap_bytes = state_bytes;
    let snap_pos = pos;

    let max_diff = difficulty.min(MAX_DEEPSEEK_POW_DIFFICULTY);
    for nonce in 0..max_diff {
        let mut bytes = snap_bytes;
        let mut p = snap_pos;

        let nonce_str = nonce.to_string();
        for &b in nonce_str.as_bytes() {
            bytes[p] ^= b;
            p += 1;
            if p == KECCAK_BLOCK {
                permute_state_bytes(&mut bytes);
                p = 0;
            }
        }

        // SHA3-256 padding
        bytes[p] ^= 0x06;
        bytes[KECCAK_BLOCK - 1] ^= 0x80;
        permute_state_bytes(&mut bytes);

        if bytes[..32] == target {
            return nonce as i64;
        }
    }

    -1
}

pub fn solve_deepseek_pow(challenge: &DeepSeekPowChallenge) -> Result<i64> {
    if challenge.algorithm != "DeepSeekHashV1" {
        return Err(Error::other(format!("Unsupported algorithm: {}", challenge.algorithm)));
    }
    let prefix = format!("{}_{}_", challenge.salt, challenge.expire_at);
    let nonce = find_deepseek_pow_nonce(&prefix, &challenge.challenge, challenge.difficulty);
    if nonce < 0 {
        Err(Error::other("PoW solver failed to find nonce within difficulty"))
    } else {
        Ok(nonce)
    }
}

// ── Stream Content Formatting ───────────────────────────────────────────────

pub fn is_thinking_model(model: &str) -> bool {
    let lower = model.to_lowercase();
    lower.contains("think") || lower.contains("r1") || lower.contains("reason")
}

pub fn is_search_model(model: &str) -> bool {
    let lower = model.to_lowercase();
    lower.contains("search") || lower.contains("fold")
}

pub fn clean_deepseek_token(text: &str) -> String {
    let s = text.replace("FINISHED", "");
    regex::Regex::new(r"(?i)^(WEB_SEARCH|SEARCHING|SEARCH)\s*")
        .ok()
        .map(|re| re.replace(&s, "").to_string())
        .unwrap_or(s)
}

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

// ── Stream Done Terminator & Guards ─────────────────────────────────────────

pub struct FinishOnceGuard {
    finished: AtomicBool,
}

impl FinishOnceGuard {
    pub fn new() -> Self {
        FinishOnceGuard {
            finished: AtomicBool::new(false),
        }
    }

    pub fn finish_once<F>(&self, f: F)
    where
        F: FnOnce(),
    {
        if !self.finished.swap(true, Ordering::SeqCst) {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).ok();
        }
    }

    pub fn has_finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }
}

impl Default for FinishOnceGuard {
    fn default() -> Self {
        Self::new()
    }
}

pub const DEEPSEEK_FINISHED_DRAIN_MS: u64 = 750;

/// How long to wait after DeepSeek `response/status=FINISHED` for trailing
/// `search_results` before closing the OpenAI-compatible SSE. Upstreams that
/// leave the HTTP body open hang OpenAI SDK clients waiting for
/// `data: [DONE]` after `finish_reason: stop`, so the drain always closes.
///
/// JS drives this off a `setTimeout`; the Rust stream transform instead polls
/// [`FinishedDrainScheduler::tick`] per upstream chunk.
pub struct FinishedDrainScheduler {
    drain_timer: Mutex<Option<Instant>>,
    drain_ms: u64,
}

impl FinishedDrainScheduler {
    pub fn new(drain_ms: u64) -> Self {
        FinishedDrainScheduler {
            drain_timer: Mutex::new(None),
            drain_ms,
        }
    }

    /// (Re)arm the drain window. Any post-FINISHED payload extends it so late
    /// `search_results` are still captured.
    pub fn schedule_finish_after_drain(&self) {
        if let Ok(mut timer) = self.drain_timer.lock() {
            *timer = Some(Instant::now());
        }
    }

    pub fn clear_finished_drain(&self) {
        if let Ok(mut timer) = self.drain_timer.lock() {
            *timer = None;
        }
    }

    /// JS `isDrainPending` asks only whether the timer is armed.
    pub fn is_drain_pending(&self) -> bool {
        self.drain_timer
            .lock()
            .map(|t| t.is_some())
            .unwrap_or(false)
    }

    /// Drain elapsed → invoke `finish` once and clear the timer.
    /// Returns `true` when it fired (so the caller can close the stream).
    pub fn tick<F: FnMut()>(&self, mut finish: F) -> bool {
        let due = self
            .drain_timer
            .lock()
            .ok()
            .and_then(|t| t.map(|start| start.elapsed().as_millis() >= self.drain_ms as u128))
            .unwrap_or(false);
        if due {
            self.clear_finished_drain();
            finish();
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pow_exact_node_vector() {
        // Test vector from JS noble-hashes implementation:
        // prefix: "test_prefix_", nonce 42 -> "1f644c1c67b7e07e81a6d7ed04f7192a36cdf3225cb9119a45717ae9b0dd986a"
        let challenge = DeepSeekPowChallenge {
            algorithm: "DeepSeekHashV1".to_string(),
            challenge: "1f644c1c67b7e07e81a6d7ed04f7192a36cdf3225cb9119a45717ae9b0dd986a".to_string(),
            salt: "test".to_string(),
            difficulty: 100,
            expire_at: 0,
            signature: "sig".to_string(),
            target_path: "/test".to_string(),
        };
        // Prefix formed in solve_deepseek_pow is salt_expireat_ = "test_0_"
        // Let's test direct find_deepseek_pow_nonce with "test_prefix_"
        let nonce = find_deepseek_pow_nonce(
            "test_prefix_",
            "1f644c1c67b7e07e81a6d7ed04f7192a36cdf3225cb9119a45717ae9b0dd986a",
            100,
        );
        assert_eq!(nonce, 42);
    }

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
    fn test_format_stream_content_search() {
        let content = "[citation:1] hello [citation:2] world";
        let formatted = format_stream_content(content, "deepseek-search");
        assert_eq!(formatted, "[1] hello [2] world");
    }

    #[test]
    fn test_finished_drain_scheduler_states() {
        let sched = FinishedDrainScheduler::new(DEEPSEEK_FINISHED_DRAIN_MS);
        assert!(!sched.is_drain_pending(), "nothing armed yet");
        assert!(!sched.tick(|| {}), "tick without an armed window never fires");

        sched.schedule_finish_after_drain();
        assert!(sched.is_drain_pending(), "armed");
        assert!(!sched.tick(|| {}), "not yet elapsed");

        std::thread::sleep(std::time::Duration::from_millis(
            DEEPSEEK_FINISHED_DRAIN_MS + 20,
        ));
        assert!(sched.is_drain_pending(), "still armed until tick fires");
        let mut fired = 0;
        assert!(sched.tick(|| fired += 1), "fires after the window");
        assert_eq!(fired, 1);
        assert!(!sched.is_drain_pending(), "disarmed after firing");
        assert!(!sched.tick(|| fired += 1), "one-shot");
        assert_eq!(fired, 1);

        // Re-arming extends the window (late search_results keep it alive).
        sched.schedule_finish_after_drain();
        sched.clear_finished_drain();
        assert!(!sched.is_drain_pending());
    }

    #[test]
    fn test_finish_once_guard() {
        let guard = FinishOnceGuard::new();
        assert!(!guard.has_finished());

        let mut count = 0;
        guard.finish_once(|| { count += 1; });
        guard.finish_once(|| { count += 1; });
        assert_eq!(count, 1);
        assert!(guard.has_finished());
    }
}
