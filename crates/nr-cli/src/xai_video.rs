//! `9router xai video` — generate a Grok Imagine video through the local
//! 9router gateway or direct xAI endpoint and save the result as an MP4 file.
//!
//! Port of `cli/src/cli/commands/xaiVideo.js`. Flow: POST
//! `/v1/videos/generations` → poll `GET /v1/videos/{request_id}` with backoff
//! until done/failed/timeout → download `video.url` → atomic rename.
//!
//! No OAuth tokens or Authorization headers are ever printed.

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use regex::Regex;
use serde_json::{json, Value};

pub const DEFAULT_PORT: u16 = 20128;
pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_MODEL: &str = "xai/grok-imagine-video";
pub const DEFAULT_OUTPUT: &str = "video.mp4";
pub const DEFAULT_TIMEOUT_SEC: u64 = 600;
pub const DEFAULT_POLL_INTERVAL_MS: u64 = 5_000;
pub const MAX_BACKOFF_MS: u64 = 60_000;

/// Statuses that end a job (the JS `TERMINAL_STATUSES` set).
pub const TERMINAL_STATUSES: &[&str] = &["done", "completed", "failed", "error", "expired", "cancelled"];
/// Terminal statuses that mean the job did not produce a video.
pub const FAILED_STATUSES: &[&str] = &["failed", "error", "expired", "cancelled"];

pub const HELP: &str = r#"Usage: 9router xai video --prompt "..." [options]

Generate a Grok Imagine video via your local 9router gateway
(requires a connected xAI account — Grok Build OAuth or API key).

Options:
  --prompt <text>         Video description (required)
  --output <file>         Output MP4 path (default: video.mp4)
  --model <id>            Model (default: xai/grok-imagine-video)
  --duration <seconds>    Video duration
  --aspect-ratio <ratio>  e.g. 16:9, 9:16, 1:1
  --resolution <res>      480p | 720p | 1080p
  --image <path-or-url>   Image input for image-to-video
  --timeout <seconds>     Max wait for the job (default: 600)
  --port <port>           Gateway port (default: 20128)
  --host <host>           Gateway host (default: 127.0.0.1)
  --api-key <key>         9router API key (or env NINE_ROUTER_API_KEY)
  -h, --help              Show this help
"#;

/// Parsed command line (`parseArgs` in the JS file).
#[derive(Debug, Clone, PartialEq)]
pub struct VideoOptions {
    pub prompt: Option<String>,
    pub output: String,
    pub model: String,
    pub duration: Option<u64>,
    pub aspect_ratio: Option<String>,
    pub resolution: Option<String>,
    pub image: Option<String>,
    pub timeout_sec: u64,
    pub port: u16,
    pub host: String,
    pub api_key: Option<String>,
    pub poll_interval_ms: u64,
    pub help: bool,
}

impl Default for VideoOptions {
    fn default() -> Self {
        Self {
            prompt: None,
            output: DEFAULT_OUTPUT.into(),
            model: DEFAULT_MODEL.into(),
            duration: None,
            aspect_ratio: None,
            resolution: None,
            image: None,
            timeout_sec: DEFAULT_TIMEOUT_SEC,
            port: DEFAULT_PORT,
            host: DEFAULT_HOST.into(),
            api_key: std::env::var("NINE_ROUTER_API_KEY").ok().filter(|s| !s.is_empty()),
            poll_interval_ms: DEFAULT_POLL_INTERVAL_MS,
            help: false,
        }
    }
}

/// Parse the `xai video` argument list. Mirrors the JS parser exactly,
/// including its `parseInt(...) || DEFAULT` fallbacks.
pub fn parse_args(argv: &[String]) -> Result<VideoOptions, String> {
    let mut opts = VideoOptions::default();
    let mut i = 0;
    while i < argv.len() {
        let a = argv[i].as_str();
        let next = |i: &mut usize| -> Result<String, String> {
            *i += 1;
            argv.get(*i).cloned().ok_or_else(|| format!("Missing value for {a}"))
        };
        match a {
            "--prompt" => opts.prompt = Some(next(&mut i)?),
            "--output" | "-o" => opts.output = next(&mut i)?,
            "--model" => opts.model = next(&mut i)?,
            "--duration" => opts.duration = next(&mut i)?.parse::<u64>().ok().filter(|d| *d > 0),
            "--aspect-ratio" => opts.aspect_ratio = Some(next(&mut i)?),
            "--resolution" => opts.resolution = Some(next(&mut i)?),
            "--image" => opts.image = Some(next(&mut i)?),
            "--timeout" => {
                opts.timeout_sec = next(&mut i)?.parse::<u64>().ok().filter(|t| *t > 0).unwrap_or(DEFAULT_TIMEOUT_SEC)
            }
            "--port" | "-p" => {
                opts.port = next(&mut i)?.parse::<u16>().ok().filter(|p| *p > 0).unwrap_or(DEFAULT_PORT)
            }
            "--host" | "-H" => {
                let host = next(&mut i)?;
                opts.host = if host.is_empty() { DEFAULT_HOST.into() } else { host };
            }
            "--api-key" => opts.api_key = Some(next(&mut i)?),
            "--poll-interval-ms" => {
                opts.poll_interval_ms = next(&mut i)?
                    .parse::<u64>()
                    .ok()
                    .filter(|p| *p > 0)
                    .unwrap_or(DEFAULT_POLL_INTERVAL_MS)
            }
            "-h" | "--help" => opts.help = true,
            other => return Err(format!("Unknown option: {other}")),
        }
        i += 1;
    }
    Ok(opts)
}

/// Strip secrets out of anything headed for the terminal.
pub fn sanitize_text(text: &str) -> String {
    static RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)Bearer\s+[A-Za-z0-9._~+/=-]{8,}").expect("static regex")
    });
    RE.replace_all(text, "Bearer [redacted]").into_owned()
}

/// Local file path → base64 data URL; URLs pass through untouched.
pub fn image_input_to_url(input: &str) -> Result<String, String> {
    let lower = input.to_ascii_lowercase();
    if lower.starts_with("http:") || lower.starts_with("https:") || lower.starts_with("data:") {
        return Ok(input.to_string());
    }
    let bytes = fs::read(input).map_err(|e| format!("Cannot read image {input}: {e}"))?;
    let mime = if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else {
        "image/jpeg"
    };
    Ok(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
}

/// Request body for `POST /v1/videos/generations`.
pub fn build_create_body(opts: &VideoOptions) -> Result<Value, String> {
    let prompt = opts.prompt.as_deref().unwrap_or("");
    let mut body = json!({ "model": opts.model, "prompt": prompt });
    if let Some(d) = opts.duration {
        body["duration"] = json!(d);
    }
    if let Some(ratio) = &opts.aspect_ratio {
        body["aspect_ratio"] = json!(ratio);
    }
    if let Some(res) = &opts.resolution {
        body["resolution"] = json!(res);
    }
    if let Some(image) = &opts.image {
        body["image"] = json!({ "url": image_input_to_url(image)? });
    }
    Ok(body)
}

/// Base URL the CLI talks to. Supports explicit `http://` / `https://` schemas or `host:port`.
pub fn gateway_url(host: &str, port: u16) -> String {
    if host.starts_with("http://") || host.starts_with("https://") {
        return host.trim_end_matches('/').to_string();
    }
    if port == 443 {
        format!("https://{host}")
    } else {
        format!("http://{host}:{port}")
    }
}

/// What the next poll should do, given the gateway's HTTP status and body.
#[derive(Debug, Clone, PartialEq)]
pub enum Classification {
    /// Still running; carries the status label and optional progress percent.
    Pending(String, Option<f64>),
    /// Job finished; the body is the final result.
    Done,
    /// Job finished in a failed state, with a sanitized message.
    Failed(String),
    /// Transient failure (429/503 or an unexpected status) — keep polling with backoff.
    Retry,
    /// Fatal response; carries the sanitized raw body.
    HardError(String),
}

/// Classify a poll response (`if (res.status === 200 && res.body) …` block).
pub fn classify_poll(status: u16, body: Option<&Value>, raw: &str) -> Classification {
    if status == 200 {
        if let Some(body) = body {
            let state = body
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase();
            if FAILED_STATUSES.contains(&state.as_str()) {
                let msg = match body.get("error") {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Object(_)) => body
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("video generation failed")
                        .to_string(),
                    Some(other) => other.to_string(),
                    None => "video generation failed".to_string(),
                };
                return Classification::Failed(sanitize_text(&msg));
            }
            if TERMINAL_STATUSES.contains(&state.as_str()) {
                return Classification::Done;
            }
            let progress = body.get("progress").and_then(|p| p.as_f64().or_else(|| p.as_i64().map(|i| i as f64)));
            return Classification::Pending(state, progress);
        }
        return Classification::Retry;
    }
    if status >= 400 && status != 429 && status != 503 {
        return Classification::HardError(sanitize_text(&raw.chars().take(300).collect::<String>()));
    }
    Classification::Retry
}

/// Extract video URL from candidate response paths.
/// Supports `video.url`, `video.file_output.public_url`, `url`, `data[0].url`, `file_output.public_url`.
pub fn video_url(body: &Value) -> Option<String> {
    if let Some(video) = body.get("video") {
        if let Some(url) = video.get("url").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            return Some(url.to_string());
        }
        if let Some(url) = video
            .get("file_output")
            .and_then(|f| f.get("public_url"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return Some(url.to_string());
        }
    }
    if let Some(url) = body.get("url").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        return Some(url.to_string());
    }
    if let Some(data) = body.get("data").and_then(Value::as_array) {
        if let Some(url) = data.first().and_then(|item| item.get("url")).and_then(Value::as_str).filter(|s| !s.is_empty()) {
            return Some(url.to_string());
        }
    }
    body.get("file_output")
        .and_then(|f| f.get("public_url"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Temp download path: `<output>.part`.
pub fn part_path(output: &str) -> String {
    format!("{output}.part")
}

// ── transport ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CreateResponse {
    pub status: u16,
    pub body: Option<Value>,
    pub raw: String,
    pub connection_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PollResponse {
    pub status: u16,
    pub body: Option<Value>,
    pub raw: String,
    pub retry_after_sec: Option<u64>,
}

impl PollResponse {
    pub fn new(status: u16, body: Option<Value>, raw: String) -> Self {
        Self { status, body, raw, retry_after_sec: None }
    }

    pub fn with_retry_after(status: u16, body: Option<Value>, raw: String, retry_after_sec: Option<u64>) -> Self {
        Self { status, body, raw, retry_after_sec }
    }
}

/// The three network calls the command makes. A trait so tests can drive the
/// poll/download flow without a socket.
pub trait VideoApi {
    fn create(&self, body: &Value) -> Result<CreateResponse, String>;
    fn poll(&self, request_id: &str, connection_id: Option<&str>) -> Result<PollResponse, String>;
    fn download(&self, url: &str, dest: &Path) -> Result<(), String>;
}

/// Blocking HTTP client for the local gateway or direct xAI endpoint.
pub struct HttpApi {
    client: reqwest::blocking::Client,
    base: String,
    api_key: Option<String>,
}

impl HttpApi {
    pub fn new(host: &str, port: u16, api_key: Option<String>) -> Result<Self, String> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self { client, base: gateway_url(host, port), api_key })
    }

    fn auth(&self, req: reqwest::blocking::RequestBuilder) -> reqwest::blocking::RequestBuilder {
        match &self.api_key {
            Some(key) if !key.is_empty() => req.bearer_auth(key),
            _ => req,
        }
    }
}

fn parse_json_or_none(raw: &str) -> Option<Value> {
    if raw.is_empty() {
        None
    } else {
        serde_json::from_str(raw).ok()
    }
}

impl VideoApi for HttpApi {
    fn create(&self, body: &Value) -> Result<CreateResponse, String> {
        let resp = self
            .auth(self.client.post(format!("{}/v1/videos/generations", self.base)).header("Accept", "application/json"))
            .json(body)
            .send()
            .map_err(|e| format!("Network error: {e}"))?;
        let status = resp.status().as_u16();
        let connection_id = resp
            .headers()
            .get("x-9router-connection-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let raw = resp.text().map_err(|e| format!("Network error: {e}"))?;
        Ok(CreateResponse { status, body: parse_json_or_none(&raw), raw, connection_id })
    }

    fn poll(&self, request_id: &str, connection_id: Option<&str>) -> Result<PollResponse, String> {
        let url = format!("{}/v1/videos/{}", self.base, urlencoding::encode(request_id));
        let mut req = self.auth(self.client.get(url).header("Accept", "application/json"));
        if let Some(id) = connection_id.filter(|s| !s.is_empty()) {
            req = req.header("x-connection-id", id);
        }
        let resp = req.send().map_err(|e| format!("Network error: {e}"))?;
        let status = resp.status().as_u16();
        let retry_after_sec = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        let raw = resp.text().map_err(|e| format!("Network error: {e}"))?;
        Ok(PollResponse { status, body: parse_json_or_none(&raw), raw, retry_after_sec })
    }

    fn download(&self, url: &str, dest: &Path) -> Result<(), String> {
        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(5))
            .timeout(Duration::from_secs(600))
            .build()
            .map_err(|e| e.to_string())?;
        let mut resp = client.get(url).send().map_err(|e| format!("Download failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("Download failed: HTTP {}", resp.status().as_u16()));
        }
        let mut file = fs::File::create(dest).map_err(|e| format!("Download failed: {e}"))?;
        io::copy(&mut resp, &mut file).map_err(|e| format!("Download failed: {e}"))?;
        Ok(())
    }
}

// ── run ─────────────────────────────────────────────────────────────────────

static CANCELLED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_sig: libc::c_int) {
    CANCELLED.store(true, Ordering::SeqCst);
}

#[cfg(unix)]
fn install_sigint() {
    unsafe {
        libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t);
    }
}

#[cfg(not(unix))]
fn install_sigint() {}

fn cancelled() -> bool {
    CANCELLED.load(Ordering::SeqCst)
}

/// Entry point for `9router xai video …`. Returns the process exit code.
pub fn run(argv: &[String]) -> i32 {
    let opts = match parse_args(argv) {
        Ok(o) => o,
        Err(err) => {
            eprintln!("❌ {err}");
            print!("{HELP}");
            return 1;
        }
    };
    if opts.help {
        print!("{HELP}");
        return 0;
    }
    if opts.prompt.as_deref().unwrap_or("").trim().is_empty() {
        eprintln!("❌ --prompt is required");
        print!("{HELP}");
        return 1;
    }
    let api = match HttpApi::new(&opts.host, opts.port, opts.api_key.clone()) {
        Ok(api) => api,
        Err(e) => {
            eprintln!("❌ {e}");
            return 1;
        }
    };
    install_sigint();
    CANCELLED.store(false, Ordering::SeqCst);
    let stdout = io::stdout();
    let mut out = stdout.lock();
    run_with(&api, &opts, &mut out)
}

/// Poll + download flow, decoupled from the transport for testing.
pub fn run_with<A: VideoApi>(api: &A, opts: &VideoOptions, out: &mut dyn Write) -> i32 {
    let part = part_path(&opts.output);
    let body = match build_create_body(opts) {
        Ok(b) => b,
        Err(e) => {
            let _ = writeln!(out, "❌ {e}");
            return 1;
        }
    };

    let _ = writeln!(out, "🎬 Requesting video ({})…", opts.model);
    let create = match api.create(&body) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(out, "❌ {}", sanitize_text(&e));
            return 1;
        }
    };

    if create.status != 200 {
        report_create_failure(create.status, create.body.as_ref(), &create.raw, out);
        return 1;
    }
    let Some(request_id) = create.body.as_ref().and_then(|b| b.get("request_id")).and_then(Value::as_str) else {
        report_create_failure(create.status, create.body.as_ref(), &create.raw, out);
        return 1;
    };
    let request_id = request_id.to_string();
    let _ = writeln!(out, "📋 Job accepted: {request_id}");

    let deadline = Instant::now() + Duration::from_secs(opts.timeout_sec);
    let mut last_line = String::new();
    let mut backoff_ms = opts.poll_interval_ms;
    let max_backoff = MAX_BACKOFF_MS.max(opts.poll_interval_ms * 4);

    loop {
        if cancelled() {
            let _ = fs::remove_file(&part);
            let _ = writeln!(out, "\n✋ Cancelled");
            return 130;
        }
        if Instant::now() > deadline {
            let _ = fs::remove_file(&part);
            let _ = writeln!(
                out,
                "❌ Timed out after {}s waiting for video job {request_id}",
                opts.timeout_sec
            );
            return 1;
        }

        let res = match api.poll(&request_id, create.connection_id.as_deref()) {
            Ok(r) => r,
            Err(e) => {
                let _ = fs::remove_file(&part);
                let _ = writeln!(out, "❌ {}", sanitize_text(&e));
                return 1;
            }
        };

        let sleep_ms = match classify_poll(res.status, res.body.as_ref(), &res.raw) {
            Classification::Failed(msg) => {
                let _ = fs::remove_file(&part);
                let _ = writeln!(out, "❌ Job {request_id} failed: {msg}");
                let lower = msg.to_ascii_lowercase();
                if lower.contains("quota") || lower.contains("rate limit") || lower.contains("credits") {
                    let _ = writeln!(out, "   xAI quota or rate limit exceeded.");
                }
                return 1;
            }
            Classification::HardError(raw) => {
                let _ = fs::remove_file(&part);
                let _ = writeln!(out, "❌ Polling failed (HTTP {}): {raw}", res.status);
                let lower = raw.to_ascii_lowercase();
                if res.status == 429 || lower.contains("quota") || lower.contains("rate limit") {
                    let _ = writeln!(out, "   xAI quota or rate limit reached. Please check your credits or tier.");
                }
                return 1;
            }
            Classification::Done => {
                let body = res.body.unwrap_or(Value::Null);
                let _ = writeln!(out);
                return finish_download(api, &body, &opts.output, &part, out);
            }
            Classification::Pending(status, progress) => {
                // Successful poll resets retry backoff
                backoff_ms = opts.poll_interval_ms;
                let line = match progress {
                    Some(p) => format!("⏳ {} {}%", if status.is_empty() { "pending" } else { &status }, p),
                    None => format!("⏳ {}", if status.is_empty() { "pending" } else { &status }),
                };
                if line != last_line {
                    last_line = line.clone();
                    write_progress(out, &line);
                }
                opts.poll_interval_ms
            }
            Classification::Retry => {
                let wait = if let Some(sec) = res.retry_after_sec {
                    (sec * 1000).max(opts.poll_interval_ms)
                } else {
                    let current = backoff_ms;
                    backoff_ms = (backoff_ms * 2).min(max_backoff);
                    current
                };
                wait
            }
        };

        sleep_interruptible(sleep_ms);
    }
}

fn write_progress(out: &mut dyn Write, line: &str) {
    if stdout_is_tty() {
        let _ = write!(out, "\r\x1b[K{line}");
        let _ = out.flush();
    } else {
        let _ = writeln!(out, "{line}");
    }
}

fn finish_download<A: VideoApi>(api: &A, body: &Value, output: &str, part: &str, out: &mut dyn Write) -> i32 {
    let Some(url) = video_url(body) else {
        let _ = writeln!(out, "❌ Job finished but no video URL was returned");
        return 1;
    };
    let _ = writeln!(out, "⬇️  Downloading…");
    let part_path = Path::new(part);
    if let Err(e) = api.download(&url, part_path) {
        let _ = fs::remove_file(part_path);
        let _ = writeln!(out, "❌ {}", sanitize_text(&e));
        return 1;
    }
    if let Err(e) = fs::rename(part_path, output) {
        let _ = fs::remove_file(part_path);
        let _ = writeln!(out, "❌ {}", sanitize_text(&e.to_string()));
        return 1;
    }
    let _ = writeln!(out, "✅ Saved {output}");
    0
}

fn report_create_failure(status: u16, body: Option<&Value>, raw: &str, out: &mut dyn Write) {
    let from_error = body.and_then(|b| b.get("error")).and_then(|e| match e {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Object(_) => e
            .get("message")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        _ => None,
    });
    let detail = from_error
        .or_else(|| body.and_then(|b| b.get("error")).map(|e| match e {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| if raw.is_empty() { format!("HTTP {status}") } else { raw.to_string() });
    let clean = sanitize_text(&detail);
    let trimmed: String = clean.chars().take(500).collect();
    let _ = writeln!(out, "❌ Create failed: {trimmed}");
    let lower = detail.to_ascii_lowercase();
    if status == 400 && lower.contains("no credentials") {
        let _ = writeln!(out, "   Connect an xAI account first: dashboard → Providers → xAI (Grok).");
    } else if status == 429 || lower.contains("quota") || lower.contains("rate limit") || lower.contains("insufficient") {
        let _ = writeln!(out, "   xAI quota or rate limit reached. Please check your credits or tier.");
    }
}

fn sleep_interruptible(ms: u64) {
    // Sleep in small slices so Ctrl-C is honoured promptly.
    let mut left = ms;
    while left > 0 {
        if cancelled() {
            return;
        }
        let slice = left.min(100);
        std::thread::sleep(Duration::from_millis(slice));
        left -= slice;
    }
}

fn stdout_is_tty() -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn defaults_match_the_js_parser() {
        let o = parse_args(&[]).unwrap();
        assert_eq!(o.model, "xai/grok-imagine-video");
        assert_eq!(o.output, "video.mp4");
        assert_eq!(o.timeout_sec, 600);
        assert_eq!(o.port, 20128);
        assert_eq!(o.host, "127.0.0.1");
        assert_eq!(o.poll_interval_ms, 5000);
        assert!(o.prompt.is_none());
    }

    #[test]
    fn parse_reads_every_flag_and_alias() {
        let o = parse_args(&args(&[
            "--prompt", "a cat surfing",
            "-o", "out.mp4",
            "--model", "xai/grok-imagine-video-1.5",
            "--duration", "8",
            "--aspect-ratio", "16:9",
            "--resolution", "720p",
            "--timeout", "30",
            "-p", "3000",
            "-H", "10.0.0.2",
            "--api-key", "sk-test",
            "--poll-interval-ms", "250",
        ]))
        .unwrap();
        assert_eq!(o.prompt.as_deref(), Some("a cat surfing"));
        assert_eq!(o.output, "out.mp4");
        assert_eq!(o.model, "xai/grok-imagine-video-1.5");
        assert_eq!(o.duration, Some(8));
        assert_eq!(o.aspect_ratio.as_deref(), Some("16:9"));
        assert_eq!(o.resolution.as_deref(), Some("720p"));
        assert_eq!(o.timeout_sec, 30);
        assert_eq!(o.port, 3000);
        assert_eq!(o.host, "10.0.0.2");
        assert_eq!(o.api_key.as_deref(), Some("sk-test"));
        assert_eq!(o.poll_interval_ms, 250);
    }

    #[test]
    fn invalid_numbers_fall_back_like_parse_int_or_default() {
        let o = parse_args(&args(&["--timeout", "abc", "--port", "nope", "--duration", "x", "--poll-interval-ms", "-1"])).unwrap();
        assert_eq!(o.timeout_sec, DEFAULT_TIMEOUT_SEC);
        assert_eq!(o.port, DEFAULT_PORT);
        assert_eq!(o.duration, None, "NaN is falsy in JS, so the field is omitted");
        assert_eq!(o.poll_interval_ms, DEFAULT_POLL_INTERVAL_MS);
    }

    #[test]
    fn unknown_option_is_an_error_and_missing_value_too() {
        assert_eq!(parse_args(&args(&["--bogus"])).unwrap_err(), "Unknown option: --bogus");
        assert!(parse_args(&args(&["--prompt"])).is_err());
        assert!(parse_args(&args(&["-h"])).unwrap().help);
    }

    #[test]
    fn create_body_only_includes_asked_fields() {
        let o = parse_args(&args(&["--prompt", "p"])).unwrap();
        let body = build_create_body(&o).unwrap();
        assert_eq!(body, json!({ "model": "xai/grok-imagine-video", "prompt": "p" }));
        assert!(body.get("duration").is_none());
        assert!(body.get("image").is_none());

        let o = parse_args(&args(&[
            "--prompt", "p",
            "--duration", "6",
            "--aspect-ratio", "9:16",
            "--resolution", "1080p",
            "--image", "https://example.com/cat.png",
        ]))
        .unwrap();
        assert_eq!(
            build_create_body(&o).unwrap(),
            json!({
                "model": "xai/grok-imagine-video",
                "prompt": "p",
                "duration": 6,
                "aspect_ratio": "9:16",
                "resolution": "1080p",
                "image": { "url": "https://example.com/cat.png" },
            })
        );
    }

    #[test]
    fn image_input_reads_local_files_and_passes_urls_through() {
        assert_eq!(image_input_to_url("http://x/y.png").unwrap(), "http://x/y.png");
        assert_eq!(image_input_to_url("HTTPS://x/y.jpg").unwrap(), "HTTPS://x/y.jpg");
        assert_eq!(image_input_to_url("data:image/png;base64,AAA").unwrap(), "data:image/png;base64,AAA");

        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("shot.png");
        fs::write(&png, [0x89u8, b'P', b'N', b'G']).unwrap();
        let url = image_input_to_url(png.to_str().unwrap()).unwrap();
        assert!(url.starts_with("data:image/png;base64,"), "got {url}");

        let jpg = dir.path().join("shot.JPG");
        fs::write(&jpg, b"jpg").unwrap();
        assert!(image_input_to_url(jpg.to_str().unwrap()).unwrap().starts_with("data:image/jpeg;base64,"));

        let missing = dir.path().join("nope.png");
        assert!(image_input_to_url(missing.to_str().unwrap()).is_err());
    }

    #[test]
    fn sanitize_redacts_bearer_tokens_only() {
        assert_eq!(sanitize_text("Bearer abcdefghijkl"), "Bearer [redacted]");
        assert_eq!(sanitize_text("bearer abcdefgh"), "Bearer [redacted]");
        assert_eq!(sanitize_text("Bearer short"), "Bearer short", "under 8 chars is left alone");
        assert_eq!(sanitize_text("no secrets here"), "no secrets here");
    }

    #[test]
    fn classify_covers_terminal_pending_and_transient_states() {
        for s in ["done", "completed", "DONE"] {
            assert_eq!(classify_poll(200, Some(&json!({"status": s})), ""), Classification::Done);
        }
        match classify_poll(200, Some(&json!({"status": "failed", "error": {"message": "nope"}})), "") {
            Classification::Failed(m) => assert_eq!(m, "nope"),
            other => panic!("expected Failed, got {other:?}"),
        }
        match classify_poll(200, Some(&json!({"status": "expired"})), "") {
            Classification::Failed(m) => assert_eq!(m, "video generation failed"),
            other => panic!("expected Failed, got {other:?}"),
        }
        assert_eq!(
            classify_poll(200, Some(&json!({"status": "running", "progress": 40})), ""),
            Classification::Pending("running".into(), Some(40.0))
        );
        assert_eq!(
            classify_poll(200, Some(&json!({})), ""),
            Classification::Pending(String::new(), None)
        );
        assert_eq!(classify_poll(429, None, "slow down"), Classification::Retry);
        assert_eq!(classify_poll(503, None, "unavailable"), Classification::Retry);
        assert_eq!(
            classify_poll(500, None, &"x".repeat(400)),
            Classification::HardError("x".repeat(300))
        );
        assert_eq!(classify_poll(200, None, ""), Classification::Retry);
    }

    #[test]
    fn video_url_prefers_url_then_file_output_and_data_array() {
        assert_eq!(video_url(&json!({"video": {"url": "https://v/1.mp4"}})).as_deref(), Some("https://v/1.mp4"));
        assert_eq!(
            video_url(&json!({"video": {"file_output": {"public_url": "https://v/2.mp4"}}})).as_deref(),
            Some("https://v/2.mp4")
        );
        assert_eq!(video_url(&json!({"url": "https://v/3.mp4"})).as_deref(), Some("https://v/3.mp4"));
        assert_eq!(
            video_url(&json!({"data": [{"url": "https://v/4.mp4"}]})).as_deref(),
            Some("https://v/4.mp4")
        );
        assert!(video_url(&json!({"video": {"url": ""}})).is_none());
        assert!(video_url(&json!({})).is_none());
    }

    #[test]
    fn part_path_is_output_plus_suffix() {
        assert_eq!(part_path("video.mp4"), "video.mp4.part");
        assert_eq!(part_path("/tmp/a b.mp4"), "/tmp/a b.mp4.part");
    }

    // ── poll + download flow with a fake gateway ────────────────────────────

    struct FakeApi {
        polls: std::cell::RefCell<Vec<(String, Option<String>)>>,
        responses: std::cell::RefCell<Vec<PollResponse>>,
        create: CreateResponse,
        download: std::cell::RefCell<Option<Result<(), String>>>,
        downloaded: std::cell::RefCell<Option<(String, std::path::PathBuf)>>,
    }

    impl FakeApi {
        fn new(responses: Vec<PollResponse>) -> Self {
            Self {
                polls: Default::default(),
                responses: std::cell::RefCell::new(responses),
                create: CreateResponse {
                    status: 200,
                    body: Some(json!({"request_id": "req 1"})),
                    raw: String::new(),
                    connection_id: Some("conn-9".into()),
                },
                download: std::cell::RefCell::new(Some(Ok(()))),
                downloaded: Default::default(),
            }
        }
    }

    impl VideoApi for FakeApi {
        fn create(&self, body: &Value) -> Result<CreateResponse, String> {
            assert_eq!(body["prompt"], "p");
            Ok(CreateResponse {
                status: self.create.status,
                body: self.create.body.clone(),
                raw: self.create.raw.clone(),
                connection_id: self.create.connection_id.clone(),
            })
        }
        fn poll(&self, request_id: &str, connection_id: Option<&str>) -> Result<PollResponse, String> {
            self.polls.borrow_mut().push((request_id.to_string(), connection_id.map(str::to_string)));
            Ok(self.responses.borrow_mut().remove(0))
        }
        fn download(&self, url: &str, dest: &Path) -> Result<(), String> {
            *self.downloaded.borrow_mut() = Some((url.to_string(), dest.to_path_buf()));
            let res = self.download.borrow().clone().unwrap_or(Ok(()));
            if res.is_ok() {
                let _ = fs::write(dest, b"video-content");
            }
            res
        }
    }

    fn opts(output: &str, poll_ms: u64) -> VideoOptions {
        VideoOptions {
            prompt: Some("p".into()),
            output: output.into(),
            poll_interval_ms: poll_ms,
            ..VideoOptions::default()
        }
    }

    #[test]
    fn happy_path_polls_until_done_then_downloads_the_video_url() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.mp4");
        let api = FakeApi::new(vec![
            PollResponse::new(200, Some(json!({"status": "queued"})), String::new()),
            PollResponse::new(200, Some(json!({"status": "running", "progress": 50})), String::new()),
            PollResponse::new(
                200,
                Some(json!({"status": "done", "video": {"url": "https://cdn/v.mp4"}})),
                String::new(),
            ),
        ]);
        let mut buf = Vec::new();
        let code = run_with(&api, &opts(output.to_str().unwrap(), 1), &mut buf);
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&buf));
        let text = String::from_utf8_lossy(&buf);
        assert!(text.contains("📋 Job accepted: req 1"), "{text}");
        assert!(text.contains("⬇️  Downloading…"));
        assert!(text.contains("✅ Saved"));
        assert!(output.exists());

        let polls = api.polls.borrow();
        assert_eq!(polls.len(), 3);
        assert!(polls.iter().all(|(id, _)| id == "req 1"));
        assert_eq!(polls[0].1.as_deref(), Some("conn-9"), "polls carry the creating account");
        assert_eq!(
            api.downloaded.borrow().as_ref().unwrap(),
            &("https://cdn/v.mp4".to_string(), std::path::PathBuf::from(format!("{}.part", output.display())))
        );
    }

    #[test]
    fn polling_with_retry_and_backoff_then_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("retry_out.mp4");
        let api = FakeApi::new(vec![
            PollResponse::with_retry_after(429, None, "Too Many Requests".into(), Some(1)),
            PollResponse::new(503, None, "Service Unavailable".into()),
            PollResponse::new(200, Some(json!({"status": "done", "url": "https://cdn/final.mp4"})), String::new()),
        ]);
        let mut buf = Vec::new();
        let code = run_with(&api, &opts(output.to_str().unwrap(), 5), &mut buf);
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&buf));
        assert!(output.exists());
        assert_eq!(api.polls.borrow().len(), 3);
    }

    #[test]
    fn failed_job_reports_and_exits_nonzero() {
        let api = FakeApi::new(vec![PollResponse::new(
            200,
            Some(json!({"status": "failed", "error": "moderation"})),
            String::new(),
        )]);
        let mut buf = Vec::new();
        assert_eq!(run_with(&api, &opts("unused.mp4", 1), &mut buf), 1);
        let text = String::from_utf8_lossy(&buf);
        assert!(text.contains("Job req 1 failed: moderation"), "{text}");
        assert!(api.downloaded.borrow().is_none());
    }

    #[test]
    fn quota_error_during_creation_shows_quota_hint() {
        let mut api = FakeApi::new(vec![]);
        api.create = CreateResponse {
            status: 429,
            body: Some(json!({"error": {"message": "insufficient_quota: please upgrade your tier"}})),
            raw: String::new(),
            connection_id: None,
        };
        let mut buf = Vec::new();
        assert_eq!(run_with(&api, &opts("x.mp4", 1), &mut buf), 1);
        let text = String::from_utf8_lossy(&buf);
        assert!(text.contains("Create failed: insufficient_quota"), "{text}");
        assert!(text.contains("xAI quota or rate limit reached"), "{text}");
    }

    #[test]
    fn create_failure_prints_provider_detail_and_credentials_hint() {
        let mut api = FakeApi::new(vec![]);
        api.create = CreateResponse {
            status: 400,
            body: Some(json!({"error": {"message": "No credentials for provider: xai"}})),
            raw: String::new(),
            connection_id: None,
        };
        let mut buf = Vec::new();
        assert_eq!(run_with(&api, &opts("x.mp4", 1), &mut buf), 1);
        let text = String::from_utf8_lossy(&buf);
        assert!(text.contains("Create failed: No credentials for provider: xai"), "{text}");
        assert!(text.contains("Connect an xAI account first"), "{text}");
    }

    #[test]
    fn transport_error_and_download_failure_are_reported() {
        struct Failing;
        impl VideoApi for Failing {
            fn create(&self, _: &Value) -> Result<CreateResponse, String> {
                Ok(CreateResponse {
                    status: 200,
                    body: Some(json!({"request_id": "r"})),
                    raw: String::new(),
                    connection_id: None,
                })
            }
            fn poll(&self, _: &str, _: Option<&str>) -> Result<PollResponse, String> {
                Ok(PollResponse::new(
                    200,
                    Some(json!({"status": "done", "video": {"url": "https://cdn/v.mp4"}})),
                    String::new(),
                ))
            }
            fn download(&self, _: &str, dest: &Path) -> Result<(), String> {
                fs::write(dest, b"partial").unwrap();
                Err("Download failed: HTTP 404".into())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("v.mp4");
        let mut buf = Vec::new();
        assert_eq!(run_with(&Failing, &opts(output.to_str().unwrap(), 1), &mut buf), 1);
        let text = String::from_utf8_lossy(&buf);
        assert!(text.contains("Download failed: HTTP 404"), "{text}");
        assert!(!Path::new(&part_path(output.to_str().unwrap())).exists(), "temp file removed on failure");
        assert!(!output.exists(), "no partial output left behind");
    }

    #[test]
    fn done_without_a_video_url_is_an_error() {
        let api = FakeApi::new(vec![PollResponse::new(
            200,
            Some(json!({"status": "completed"})),
            String::new(),
        )]);
        let mut buf = Vec::new();
        assert_eq!(run_with(&api, &opts("x.mp4", 1), &mut buf), 1);
        assert!(String::from_utf8_lossy(&buf).contains("no video URL was returned"));
    }

    #[test]
    fn gateway_url_matches_the_js_origin() {
        assert_eq!(gateway_url("127.0.0.1", 20128), "http://127.0.0.1:20128");
        assert_eq!(gateway_url("localhost", 3000), "http://localhost:3000");
        assert_eq!(gateway_url("https://api.x.ai", 443), "https://api.x.ai");
        assert_eq!(gateway_url("api.x.ai", 443), "https://api.x.ai");
    }

    // ── Mock integration tests with local socket ────────────────────────────

    const FAKE_MP4: &[u8] = b"FAKE-MP4-STREAM-DATA-TEST";

    fn start_test_server<F>(handler: F) -> (u16, std::sync::Arc<AtomicBool>, std::thread::JoinHandle<()>)
    where
        F: Fn(&str, &str, &[u8]) -> (u16, Vec<(&'static str, String)>, Vec<u8>) + Send + Sync + 'static,
    {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let port = listener.local_addr().expect("port").port();
        let handler = std::sync::Arc::new(handler);
        let running = std::sync::Arc::new(AtomicBool::new(true));
        let running_clone = running.clone();

        let handle = std::thread::spawn(move || {
            listener.set_nonblocking(true).expect("set nonblocking");
            while running_clone.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut buf = [0u8; 4096];
                        let n = match stream.read(&mut buf) {
                            Ok(n) if n > 0 => n,
                            _ => continue,
                        };
                        let req_str = String::from_utf8_lossy(&buf[..n]);
                        let mut lines = req_str.lines();
                        let req_line = lines.next().unwrap_or("");
                        let parts: Vec<&str> = req_line.split_whitespace().collect();
                        let method = parts.first().copied().unwrap_or("GET");
                        let uri = parts.get(1).copied().unwrap_or("/");

                        let body_offset = req_str
                            .find("\r\n\r\n")
                            .map(|idx| idx + 4)
                            .or_else(|| req_str.find("\n\n").map(|idx| idx + 2))
                            .unwrap_or(n);
                        let body = &buf[body_offset.min(n)..n];

                        let (status, headers, resp_body) = handler(method, uri, body);
                        let status_text = match status {
                            200 => "200 OK",
                            400 => "400 Bad Request",
                            429 => "429 Too Many Requests",
                            404 => "404 Not Found",
                            503 => "503 Service Unavailable",
                            _ => "500 Internal Server Error",
                        };

                        let mut resp = format!(
                            "HTTP/1.1 {status_text}\r\nContent-Length: {}\r\nConnection: close\r\n",
                            resp_body.len()
                        );
                        for (k, v) in headers {
                            resp.push_str(&format!("{k}: {v}\r\n"));
                        }
                        resp.push_str("\r\n");

                        let _ = stream.write_all(resp.as_bytes());
                        let _ = stream.write_all(&resp_body);
                        let _ = stream.flush();
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        (port, running, handle)
    }

    #[test]
    fn mock_integration_end_to_end_flow() {
        let poll_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let poll_count_c = poll_count.clone();
        let port_holder = std::sync::Arc::new(std::sync::atomic::AtomicU16::new(0));
        let port_holder_c = port_holder.clone();

        let (port, running, handle) = start_test_server(move |method, uri, body| match (method, uri) {
            ("POST", "/v1/videos/generations") => {
                let parsed: Value = serde_json::from_slice(body).unwrap_or(json!({}));
                assert_eq!(parsed["prompt"], "a neon cat");
                (
                    200,
                    vec![
                        ("Content-Type", "application/json".into()),
                        ("x-9router-connection-id", "conn-wire-42".into()),
                    ],
                    serde_json::to_vec(&json!({ "request_id": "job-wire-1" })).unwrap(),
                )
            }
            ("GET", "/v1/videos/job-wire-1") => {
                let c = poll_count_c.fetch_add(1, Ordering::SeqCst);
                if c < 2 {
                    (
                        200,
                        vec![("Content-Type", "application/json".into())],
                        serde_json::to_vec(&json!({ "status": "running", "progress": (c + 1) * 45 })).unwrap(),
                    )
                } else {
                    let p = port_holder_c.load(Ordering::SeqCst);
                    let dl = format!("http://127.0.0.1:{p}/files/video.mp4");
                    (
                        200,
                        vec![("Content-Type", "application/json".into())],
                        serde_json::to_vec(&json!({
                            "status": "done",
                            "video": { "url": dl }
                        }))
                        .unwrap(),
                    )
                }
            }
            ("GET", "/files/video.mp4") => (
                200,
                vec![("Content-Type", "video/mp4".into())],
                FAKE_MP4.to_vec(),
            ),
            _ => (404, vec![], b"Not Found".to_vec()),
        });
        port_holder.store(port, Ordering::SeqCst);

        let dir = tempfile::tempdir().unwrap();
        let out_file = dir.path().join("wire_output.mp4");

        let opts = VideoOptions {
            prompt: Some("a neon cat".into()),
            output: out_file.to_str().unwrap().into(),
            host: "127.0.0.1".into(),
            port,
            timeout_sec: 10,
            poll_interval_ms: 10,
            ..VideoOptions::default()
        };

        let api = HttpApi::new(&opts.host, opts.port, None).unwrap();
        let mut log_buf = Vec::new();
        let code = run_with(&api, &opts, &mut log_buf);
        running.store(false, Ordering::SeqCst);
        let _ = handle.join();

        let logs = String::from_utf8_lossy(&log_buf);
        assert_eq!(code, 0, "logs: {logs}");
        assert!(out_file.exists());
        assert_eq!(fs::read(&out_file).unwrap(), FAKE_MP4);
        assert!(!Path::new(&part_path(out_file.to_str().unwrap())).exists());
    }

    #[test]
    fn mock_integration_bad_prompt_error() {
        let (port, running, handle) = start_test_server(|method, uri, _| match (method, uri) {
            ("POST", "/v1/videos/generations") => (
                400,
                vec![("Content-Type", "application/json".into())],
                serde_json::to_vec(&json!({
                    "error": { "message": "invalid_argument: prompt violates policy" }
                }))
                .unwrap(),
            ),
            _ => (404, vec![], b"".to_vec()),
        });

        let dir = tempfile::tempdir().unwrap();
        let out_file = dir.path().join("bad_prompt.mp4");

        let opts = VideoOptions {
            prompt: Some("forbidden content".into()),
            output: out_file.to_str().unwrap().into(),
            host: "127.0.0.1".into(),
            port,
            ..VideoOptions::default()
        };

        let api = HttpApi::new(&opts.host, opts.port, None).unwrap();
        let mut log_buf = Vec::new();
        let code = run_with(&api, &opts, &mut log_buf);
        running.store(false, Ordering::SeqCst);
        let _ = handle.join();

        let logs = String::from_utf8_lossy(&log_buf);
        assert_eq!(code, 1);
        assert!(logs.contains("invalid_argument: prompt violates policy"), "{logs}");
        assert!(!out_file.exists());
    }
}
