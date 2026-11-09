//! Tunnel routes. Faithful port of `src/app/api/tunnel/**` plus
//! `src/lib/tunnel/**` (cloudflared quick tunnel + Tailscale Funnel).
//!
//! State lives where the JS build keeps it: `DATA_DIR/tunnel/state.json`,
//! `DATA_DIR/tunnel/cloudflared.pid`, `DATA_DIR/bin/cloudflared`,
//! `DATA_DIR/tunnel/tailscaled.sock`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::Extension;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use nr_db::{get_settings, update_settings, Db};
use serde_json::{json, Value};

use super::api::common::get_db;

const HEALTH_INTERVAL_MS: u64 = 2000;
const CLOUDFLARE_HEALTH_TIMEOUT_MS: u64 = 60_000;
const TAILSCALE_ENABLE_TIMEOUT_MS: u64 = 20_000;
const FETCH_TIMEOUT_MS: u64 = 5000;
/// `HEALTH_CHECK.dnsTimeoutMs` (cloudflare/config.js and tailscale/config.js).
const DNS_TIMEOUT_MS: u64 = 3000;
const STATUS_CACHE_TTL_MS: u64 = 3000;
const PROBE_TTL_MS: u64 = 10_000;
const DNS_WARMUP_DELAY_MS: u64 = 8000;
const STARTUP_TIMEOUT_MS: u64 = 90_000;

// ─── shared/watchdogConfig.js ───────────────────────────────────────────────

/// Cooldown for repeating watchdog restarts (anti hammer-loop).
const RESTART_COOLDOWN_MS: u64 = 120_000;
/// Wait for DHCP/DNS to settle after a network change.
const NETWORK_SETTLE_MS: u64 = 2500;
const WATCHDOG_INTERVAL_MS: u64 = 60_000;
const NETWORK_CHECK_INTERVAL_MS: u64 = 5000;

/// `FORCE_RESTART_REASONS`: one-shot transitions bypass the cooldown.
fn is_force_restart_reason(reason: &str) -> bool {
    matches!(
        reason,
        "startup" | "netchange" | "sleep" | "sleep+netchange" | "online" | "unexpected-exit"
    )
}

fn tunnel_dir() -> PathBuf {
    nr_core::paths::data_dir().join("tunnel")
}

fn state_file() -> PathBuf {
    tunnel_dir().join("state.json")
}

fn pid_file() -> PathBuf {
    tunnel_dir().join("cloudflared.pid")
}

fn bin_dir() -> PathBuf {
    nr_core::paths::data_dir().join("bin")
}

fn cloudflared_bin() -> PathBuf {
    let name = if cfg!(windows) { "cloudflared.exe" } else { "cloudflared" };
    bin_dir().join(name)
}

fn tailscale_dir() -> PathBuf {
    nr_core::paths::data_dir().join("tailscale")
}

fn tailscale_socket() -> PathBuf {
    tailscale_dir().join("tailscaled.sock")
}

fn tailscale_bin() -> PathBuf {
    bin_dir().join(if cfg!(windows) { "tailscale.exe" } else { "tailscale" })
}

fn worker_url() -> String {
    std::env::var("TUNNEL_WORKER_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://abc-tunnel.us".into())
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn ensure_dir(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
}

// ─── shared/state.js ────────────────────────────────────────────────────────

fn load_state() -> Option<Value> {
    let text = std::fs::read_to_string(state_file()).ok()?;
    serde_json::from_str(&text).ok()
}

fn save_state(state: &Value) {
    ensure_dir(&tunnel_dir());
    let _ = std::fs::write(state_file(), serde_json::to_string_pretty(state).unwrap_or_default());
}

/// `generateShortId()`: 6 chars from `abcdefghijklmnpqrstuvwxyz23456789`.
fn generate_short_id() -> String {
    const CHARS: &[u8] = b"abcdefghijklmnpqrstuvwxyz23456789";
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..6).map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char).collect()
}

fn public_url(short_id: &str) -> String {
    if short_id.is_empty() {
        String::new()
    } else {
        format!("https://r{short_id}.abc-tunnel.us")
    }
}

// ─── pid helpers ────────────────────────────────────────────────────────────

fn save_pid(pid: u32) {
    ensure_dir(&tunnel_dir());
    let _ = std::fs::write(pid_file(), pid.to_string());
}

fn load_pid() -> Option<i64> {
    std::fs::read_to_string(pid_file()).ok()?.trim().parse().ok()
}

fn clear_pid() {
    let _ = std::fs::remove_file(pid_file());
}

fn is_pid_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    #[cfg(unix)]
    {
        unsafe { libc_kill(pid as i32, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(unix)]
unsafe fn libc_kill(pid: i32, sig: i32) -> i32 {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    kill(pid, sig)
}

fn is_cloudflared_running() -> bool {
    load_pid().map(is_pid_alive).unwrap_or(false)
}

/// `killCloudflaredByPort(port)`: kill any cloudflared whose command line
/// targets this port. The boundary class (`:20128` must not match `:201280`)
/// is what makes the pattern safe.
fn kill_cloudflared_by_port(port: u16) {
    let pattern = format!("cloudflared.*:{}[^0-9]", port);
    let _ = Command::new("pkill")
        .args(["-f", &pattern])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// `killCloudflared(localPort)`: stop the tracked child, the recorded PID, and
/// anything else serving the port.
fn kill_cloudflared(local_port: Option<u16>) {
    INTENTIONAL_KILL.store(true, std::sync::atomic::Ordering::SeqCst);
    if let Some(pid) = load_pid() {
        #[cfg(unix)]
        unsafe {
            libc_kill(pid as i32, 15)
        };
        clear_pid();
    }
    if let Some(port) = local_port {
        kill_cloudflared_by_port(port);
    }
}

/// Set while a deliberate kill is in flight so a child exit is not treated as
/// an unexpected exit (`intentionalKill` in cloudflared.js).
static INTENTIONAL_KILL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// ─── cloudflared download ───────────────────────────────────────────────────

/// `dlState` from cloudflared.js, shared with `/api/tunnel/status`.
struct DownloadState {
    downloading: bool,
    progress: u32,
}

static DOWNLOAD: std::sync::LazyLock<parking_lot::Mutex<DownloadState>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(DownloadState { downloading: false, progress: 0 }));

fn download_status() -> Value {
    let st = DOWNLOAD.lock();
    json!({ "downloading": st.downloading, "progress": st.progress })
}

fn download_url() -> Option<String> {
    const BASE: &str = "https://github.com/cloudflare/cloudflared/releases/latest/download";
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    let file = match (os, arch) {
        ("macos", "x86_64") => "cloudflared-darwin-amd64.tgz",
        ("macos", _) => "cloudflared-darwin-arm64.tgz",
        ("windows", "x86_64") => "cloudflared-windows-amd64.exe",
        ("windows", _) => "cloudflared-windows-386.exe",
        ("linux", "aarch64") => "cloudflared-linux-arm64",
        ("linux", _) => "cloudflared-linux-amd64",
        _ => return None,
    };
    Some(format!("{BASE}/{file}"))
}

fn is_valid_binary(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else { return false };
    if meta.len() < 1024 * 1024 {
        return false;
    }
    let Ok(bytes) = std::fs::read(path) else { return false };
    if bytes.len() < 4 {
        return false;
    }
    if cfg!(windows) {
        bytes.starts_with(&[0x4d, 0x5a])
    } else if cfg!(target_os = "macos") {
        bytes.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]) || bytes.starts_with(&[0xce, 0xfa, 0xed, 0xfe])
    } else {
        bytes.starts_with(&[0x7f, 0x45, 0x4c, 0x46])
    }
}

static ENSURE_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> = std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

/// `ensureCloudflared()`: return the cached binary path, downloading if needed.
async fn ensure_cloudflared() -> Result<PathBuf, String> {
    let _guard = ENSURE_LOCK.lock().await;
    let bin = cloudflared_bin();
    ensure_dir(&bin_dir());

    let tmp = bin.with_extension(format!(
        "{}.tmp",
        bin.extension().and_then(|e| e.to_str()).unwrap_or("bin")
    ));
    if tmp.exists() {
        let _ = std::fs::remove_file(&tmp);
    }

    if bin.exists() {
        if !is_valid_binary(&bin) {
            let _ = std::fs::remove_file(&bin);
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755));
            }
            return Ok(bin);
        }
    }

    let url = download_url().ok_or_else(|| format!("Unsupported platform: {}-{}", std::env::consts::OS, std::env::consts::ARCH))?;
    let is_archive = url.ends_with(".tgz");
    let dest = if is_archive { bin_dir().join("cloudflared.tgz.tmp") } else { tmp.clone() };

    download_file(&url, &dest).await?;

    if is_archive {
        let status = Command::new("tar")
            .arg("-xzf")
            .arg(&dest)
            .arg("-C")
            .arg(bin_dir())
            .status()
            .map_err(|e| format!("tar failed: {e}"))?;
        let _ = std::fs::remove_file(&dest);
        if !status.success() {
            return Err("failed to extract cloudflared archive".into());
        }
    } else {
        std::fs::rename(&dest, &bin).map_err(|e| format!("download rename failed: {e}"))?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755));
    }
    if !is_valid_binary(&bin) {
        return Err("downloaded cloudflared binary is invalid".into());
    }
    Ok(bin)
}

/// `downloadFile(url, dest)` with the shared progress state.
async fn download_file(url: &str, dest: &Path) -> Result<(), String> {
    let mut current = url.to_string();
    for _ in 0..5 {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| e.to_string())?;
        let resp = client.get(&current).send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        if status.is_redirection() {
            let Some(location) = resp.headers().get("location").and_then(|v| v.to_str().ok()) else {
                return Err(format!("redirect without location: {status}"));
            };
            current = location.to_string();
            continue;
        }
        if !status.is_success() {
            return Err(format!("Download failed with status {}", status.as_u16()));
        }
        let total = resp.content_length().unwrap_or(0);
        {
            let mut st = DOWNLOAD.lock();
            st.downloading = true;
            st.progress = 0;
        }
        let mut file = std::fs::File::create(dest).map_err(|e| {
            let mut st = DOWNLOAD.lock();
            st.downloading = false;
            e.to_string()
        })?;
        use std::io::Write;
        let mut received: u64 = 0;
        let mut resp = resp;
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    received += chunk.len() as u64;
                    if let Err(e) = file.write_all(&chunk) {
                        let mut st = DOWNLOAD.lock();
                        st.downloading = false;
                        let _ = std::fs::remove_file(dest);
                        return Err(e.to_string());
                    }
                    if total > 0 {
                        DOWNLOAD.lock().progress = ((received * 100) / total) as u32;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let mut st = DOWNLOAD.lock();
                    st.downloading = false;
                    st.progress = 0;
                    drop(file);
                    let _ = std::fs::remove_file(dest);
                    return Err(e.to_string());
                }
            }
        }
        let _ = file.flush();
        {
            let mut st = DOWNLOAD.lock();
            st.downloading = false;
            st.progress = 100;
        }
        return Ok(());
    }
    Err("too many redirects".into())
}

// ─── tunnel health probes ───────────────────────────────────────────────────

/// Build a DNS A-record query packet for `hostname` (recursion desired).
///
/// `shared/dnsResolver.js` asks public resolvers directly so the OS negative
/// cache (mDNSResponder holds NXDOMAIN) cannot make a freshly registered
/// quick tunnel look dead. Same wire format, hand-rolled to avoid a DNS dep.
fn build_dns_query(hostname: &str, txid: u16) -> Vec<u8> {
    let mut packet = Vec::with_capacity(32);
    packet.extend_from_slice(&txid.to_be_bytes());
    packet.extend_from_slice(&0x0100u16.to_be_bytes()); // RD=1
    packet.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    packet.extend_from_slice(&0u16.to_be_bytes()); // ANCOUNT
    packet.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    packet.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT
    for label in hostname.trim_end_matches('.').split('.') {
        let len = label.len().min(63);
        packet.push(len as u8);
        packet.extend_from_slice(&label.as_bytes()[..len]);
    }
    packet.push(0);
    packet.extend_from_slice(&1u16.to_be_bytes()); // QTYPE = A
    packet.extend_from_slice(&1u16.to_be_bytes()); // QCLASS = IN
    packet
}

/// `true` when the reply is a NOERROR answer with at least one A record.
fn dns_reply_resolved(reply: &[u8], txid: u16) -> bool {
    if reply.len() < 12 {
        return false;
    }
    if u16::from_be_bytes([reply[0], reply[1]]) != txid {
        return false;
    }
    let rcode = reply[3] & 0x0f;
    let answers = u16::from_be_bytes([reply[6], reply[7]]);
    rcode == 0 && answers > 0
}

/// `resolveDns(hostname, timeoutMs)`: public resolvers first, OS resolver after.
async fn resolve_dns(hostname: &str, timeout_ms: u64) -> bool {
    if hostname.is_empty() {
        return false;
    }
    // A literal IP always "resolves".
    if hostname.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }

    const PUBLIC_RESOLVERS: [&str; 3] = ["1.1.1.1:53", "1.0.0.1:53", "8.8.8.8:53"];
    for server in PUBLIC_RESOLVERS {
        let Ok(bind) = tokio::net::UdpSocket::bind("0.0.0.0:0").await else { continue };
        if bind.connect(server).await.is_err() {
            continue;
        }
        let txid = (now_ms() & 0xffff) as u16;
        let query = build_dns_query(hostname, txid);
        if bind.send(&query).await.is_err() {
            continue;
        }
        let mut buf = [0u8; 512];
        match tokio::time::timeout(Duration::from_millis(timeout_ms), bind.recv(&mut buf)).await {
            Ok(Ok(n)) if dns_reply_resolved(&buf[..n], txid) => return true,
            Ok(Ok(_)) => continue, // NOERROR/empty → try the next resolver
            _ => continue,
        }
    }

    // Fall back to the OS resolver, like `dns.promises.resolve4`.
    let lookup = tokio::net::lookup_host((hostname, 0));
    match tokio::time::timeout(Duration::from_millis(timeout_ms), lookup).await {
        Ok(Ok(mut addrs)) => {
            let mut found = false;
            for addr in addrs.by_ref() {
                if addr.is_ipv4() {
                    found = true;
                    break;
                }
            }
            found
        }
        _ => false,
    }
}

/// `probeUrlAlive(url)`: `GET {url}/api/health` returns 2xx.
async fn probe_url_alive(url: &str) -> bool {
    if url.is_empty() {
        return false;
    }
    let Ok(parsed) = url::Url::parse(url) else { return false };
    let Some(hostname) = parsed.host_str() else { return false };
    if !resolve_dns(hostname, DNS_TIMEOUT_MS).await {
        return false;
    }
    let Ok(client) = nr_core::http::client_for(None, false) else { return false };
    match tokio::time::timeout(
        Duration::from_millis(FETCH_TIMEOUT_MS),
        client.get(format!("{}/api/health", url.trim_end_matches('/'))).send(),
    )
    .await
    {
        Ok(Ok(res)) => res.status().is_success(),
        _ => false,
    }
}

/// `checkInternet()`: TCP connect to 1.1.1.1:443 within 3 s.
async fn check_internet() -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_millis(3000),
            tokio::net::TcpStream::connect(("1.1.1.1", 443))
        )
        .await,
        Ok(Ok(_))
    )
}

/// `waitForHealth(url, cancelToken)`.
async fn wait_for_health(url: &str, cancelled: &std::sync::atomic::AtomicBool, timeout_ms: u64) -> Result<(), String> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(timeout_ms) {
        if cancelled.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("cancelled".into());
        }
        if probe_url_alive(url).await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(HEALTH_INTERVAL_MS)).await;
    }
    Err(format!("Health check timeout after {timeout_ms}ms"))
}

/// `registerTunnelUrl(shortId, tunnelUrl)`.
async fn register_tunnel_url(short_id: &str, tunnel_url: &str) {
    let url = format!("{}/api/tunnel/register", worker_url().trim_end_matches('/'));
    let body = json!({ "shortId": short_id, "tunnelUrl": tunnel_url });
    let mut req = nr_core::http::FetchRequest::post_json(url, nr_core::http::HeaderList::new(), &body);
    req.header_timeout = Some(Duration::from_millis(10_000));
    let _ = nr_core::http::proxy_fetch(req, None).await;
}

// ─── tunnel service (manager.js) ────────────────────────────────────────────

fn settings_of(ext: &Option<Extension<Db>>) -> Value {
    let db = match ext {
        Some(Extension(db)) => Some(db.clone()),
        None => get_db(None).ok(),
    };
    db.as_ref().and_then(|db| get_settings(db).ok()).unwrap_or_else(|| json!({}))
}

fn write_settings(ext: &Option<Extension<Db>>, updates: Value) {
    let db = match ext {
        Some(Extension(db)) => Some(db.clone()),
        None => get_db(None).ok(),
    };
    if let Some(db) = db {
        let _ = update_settings(&db, updates);
    }
}

static CANCEL: std::sync::LazyLock<std::sync::Arc<std::sync::atomic::AtomicBool>> =
    std::sync::LazyLock::new(|| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)));

/// The last port an enable flow targeted (`svc.activeLocalPort`).
static ACTIVE_LOCAL_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
/// `svc.spawnInProgress`: an enable flow is mid-flight.
static SPAWN_IN_PROGRESS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// `svc.lastRestartAt` in epoch ms; 0 = never.
static LAST_RESTART_AT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn active_local_port() -> Option<u16> {
    match ACTIVE_LOCAL_PORT.load(std::sync::atomic::Ordering::SeqCst) {
        0 => None,
        p => Some(p),
    }
}

struct TunnelError {
    status: StatusCode,
    message: String,
}

impl TunnelError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self { status, message: message.into() }
    }
}

/// `enableTunnel(localPort)`: spawn (or reuse) a cloudflared quick tunnel.
async fn enable_tunnel(ext: &Option<Extension<Db>>, local_port: u16) -> Result<Value, TunnelError> {
    CANCEL.store(false, std::sync::atomic::Ordering::SeqCst);
    let cancel = CANCEL.clone();
    cancel.store(false, std::sync::atomic::Ordering::SeqCst);
    ACTIVE_LOCAL_PORT.store(local_port, std::sync::atomic::Ordering::SeqCst);
    SPAWN_IN_PROGRESS.store(true, std::sync::atomic::Ordering::SeqCst);

    let result = enable_tunnel_inner(ext, local_port, &cancel).await;
    SPAWN_IN_PROGRESS.store(false, std::sync::atomic::Ordering::SeqCst);
    result
}

async fn enable_tunnel_inner(
    ext: &Option<Extension<Db>>,
    local_port: u16,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<Value, TunnelError> {
    let cancel = cancel.clone();

    if is_cloudflared_running() {
        if let Some(existing) = load_state() {
            let tunnel_url = existing.get("tunnelUrl").and_then(Value::as_str).unwrap_or("").to_string();
            let short_id = existing.get("shortId").and_then(Value::as_str).unwrap_or("").to_string();
            if !tunnel_url.is_empty() && !short_id.is_empty() {
                let url = public_url(&short_id);
                let (direct_ok, public_ok) = tokio::join!(probe_url_alive(&tunnel_url), probe_url_alive(&url));
                if direct_ok && public_ok {
                    return Ok(json!({
                        "success": true,
                        "tunnelUrl": tunnel_url,
                        "shortId": short_id,
                        "publicUrl": url,
                        "alreadyRunning": true,
                    }));
                }
            }
        }
    }

    // killCloudflared(localPort)
    kill_cloudflared(Some(local_port));
    INTENTIONAL_KILL.store(false, std::sync::atomic::Ordering::SeqCst);
    if cancel.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(TunnelError::new(StatusCode::INTERNAL_SERVER_ERROR, "tunnel cancelled"));
    }

    let short_id = load_state()
        .and_then(|s| s.get("shortId").and_then(Value::as_str).map(String::from))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(generate_short_id);

    let binary = ensure_cloudflared().await.map_err(|e| TunnelError::new(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    // Quick-tunnel config placeholder, so the CLI never reads ~/.cloudflared.
    let config_dir = std::env::temp_dir().join(format!("cloudflared-quick-{}", uuid::Uuid::new_v4()));
    let _ = std::fs::create_dir_all(&config_dir);
    let config_path = config_dir.join("config.yml");
    let _ = std::fs::write(&config_path, "# quick-tunnel config placeholder\n");

    let protocol = std::env::var("TUNNEL_TRANSPORT_PROTOCOL")
        .or_else(|_| std::env::var("CLOUDFLARED_PROTOCOL"))
        .unwrap_or_else(|_| "http2".into())
        .trim()
        .to_lowercase();
    let protocol = if matches!(protocol.as_str(), "http2" | "quic" | "auto") { protocol } else { "http2".into() };

    let mut child = tokio::process::Command::new(&binary)
        .args([
            "tunnel",
            "--url",
            &format!("http://127.0.0.1:{local_port}"),
            "--config",
            &config_path.to_string_lossy(),
            "--no-autoupdate",
            "--retries",
            "99",
        ])
        .env("TUNNEL_TRANSPORT_PROTOCOL", protocol)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false)
        .spawn()
        .map_err(|e| TunnelError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to spawn cloudflared: {e}")))?;

    if let Some(pid) = child.id() {
        save_pid(pid);
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
    if let Some(stdout) = child.stdout.take() {
        let tx = tx.clone();
        tokio::spawn(pump_lines(stdout, tx));
    }
    if let Some(stderr) = child.stderr.take() {
        let tx = tx.clone();
        tokio::spawn(pump_lines(stderr, tx));
    }
    drop(tx);

    // Parse the first quick-tunnel URL out of cloudflared's log.
    let deadline = tokio::time::sleep(Duration::from_millis(STARTUP_TIMEOUT_MS));
    tokio::pin!(deadline);
    let mut log_tail = String::new();
    let mut tunnel_url: Option<String> = None;
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            line = rx.recv() => {
                let Some(line) = line else { break };
                log_tail = format!("{log_tail}{line}\n");
                if log_tail.len() > 4000 {
                    let start = log_tail.len() - 4000;
                    log_tail = log_tail[start..].to_string();
                }
                if let Some(url) = quick_tunnel_url(&line) {
                    tunnel_url = Some(url);
                    break;
                }
            }
        }
    }

    let Some(tunnel_url) = tunnel_url else {
        let _ = std::fs::remove_dir_all(&config_dir);
        let _ = child.start_kill();
        clear_pid();
        return Err(TunnelError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Quick tunnel timed out. Last log: {}", tail(&log_tail, 800)),
        ));
    };
    let _ = std::fs::remove_dir_all(&config_dir);

    let url = public_url(&short_id);
    register_tunnel_url(&short_id, &tunnel_url).await;
    save_state(&json!({ "shortId": short_id, "tunnelUrl": tunnel_url }));
    write_settings(ext, json!({ "tunnelEnabled": true, "tunnelUrl": tunnel_url }));

    wait_for_health(&url, &cancel, CLOUDFLARE_HEALTH_TIMEOUT_MS)
        .await
        .map_err(|e| TunnelError::new(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    // Direct probe is best-effort: *.trycloudflare.com DNS can lag.
    if !probe_url_alive(&tunnel_url).await {
        tracing::warn!("[Tunnel] direct URL not reachable yet, continuing via publicUrl");
    }

    Ok(json!({
        "success": true,
        "tunnelUrl": tunnel_url,
        "shortId": short_id,
        "publicUrl": url,
    }))
}

/// Forward one child stream's lines to the quick-tunnel URL scanner.
async fn pump_lines<R>(stream: R, tx: tokio::sync::mpsc::Sender<String>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if tx.send(line).await.is_err() {
            break;
        }
    }
}

fn tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.trim().to_string();
    }
    let mut start = text.len() - max;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].trim().to_string()
}

/// `getQuickTunnelUrlFromLog`: last `https://<host>.trycloudflare.com`, skipping `api`.
fn quick_tunnel_url(line: &str) -> Option<String> {
    let re = regex::Regex::new(r"https://([a-z0-9-]+)\.trycloudflare\.com").ok()?;
    re.captures_iter(line)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
        .filter(|host| host != "api")
        .last()
        .map(|host| format!("https://{host}.trycloudflare.com"))
}

/// `disableTunnel()`.
fn disable_tunnel(ext: &Option<Extension<Db>>) -> Value {
    CANCEL.store(true, std::sync::atomic::Ordering::SeqCst);
    kill_cloudflared(active_local_port());
    INTENTIONAL_KILL.store(false, std::sync::atomic::Ordering::SeqCst);
    if let Some(state) = load_state() {
        let short_id = state.get("shortId").cloned().unwrap_or(Value::Null);
        save_state(&json!({ "shortId": short_id, "tunnelUrl": Value::Null }));
    }
    write_settings(ext, json!({ "tunnelEnabled": false, "tunnelUrl": "" }));
    json!({ "success": true })
}

fn tunnel_status(ext: &Option<Extension<Db>>) -> Value {
    let settings = settings_of(ext);
    let settings_enabled = settings.get("tunnelEnabled").and_then(Value::as_bool) == Some(true);
    let state = load_state().unwrap_or(Value::Null);
    let short_id = state.get("shortId").and_then(Value::as_str).unwrap_or("").to_string();
    let tunnel_url = state.get("tunnelUrl").and_then(Value::as_str).unwrap_or("").to_string();
    let running = if settings_enabled { is_cloudflared_running() } else { false };
    json!({
        "enabled": settings_enabled && running,
        "settingsEnabled": settings_enabled,
        "tunnelUrl": tunnel_url,
        "shortId": short_id,
        "publicUrl": public_url(&short_id),
        "running": running,
    })
}

// ─── tailscale (tailscale.js) ───────────────────────────────────────────────

fn unix_tailscale_candidates() -> [&'static str; 5] {
    [
        "/usr/local/bin/tailscale",
        "/opt/homebrew/bin/tailscale",
        "/usr/sbin/tailscale",
        "/usr/bin/tailscale",
        "/snap/bin/tailscale",
    ]
}

/// `getTailscaleBin()`.
fn get_tailscale_bin() -> Option<String> {
    let local = tailscale_bin();
    if local.exists() {
        return Some(local.to_string_lossy().into_owned());
    }
    if cfg!(windows) {
        let well_known = "C:\\Program Files\\Tailscale\\tailscale.exe";
        if Path::new(well_known).exists() {
            return Some(well_known.to_string());
        }
        return find_in_path(if cfg!(windows) { "tailscale.exe" } else { "tailscale" });
    }
    unix_tailscale_candidates()
        .iter()
        .find(|p| Path::new(p).exists())
        .map(|p| (*p).to_string())
        .or_else(|| find_in_path("tailscale"))
}

/// `which <name>`: resolve a binary on `$PATH` (`bgRefreshBin` in tailscale.js
/// trusts `which` when the well-known install paths miss).
fn find_in_path(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

fn is_tailscale_installed() -> bool {
    get_tailscale_bin().is_some()
}

fn is_system_daemon_running() -> bool {
    if cfg!(windows) {
        return false;
    }
    let socket = Path::new("/var/run/tailscale/tailscaled.sock");
    if !socket.exists() {
        return false;
    }
    let Some(bin) = get_tailscale_bin() else { return false };
    let out = Command::new(&bin)
        .args(["--socket", "/var/run/tailscale/tailscaled.sock", "status", "--json"])
        .output();
    match out {
        Ok(out) if out.status.success() => serde_json::from_slice::<Value>(&out.stdout)
            .ok()
            .and_then(|j| j.get("BackendState").and_then(Value::as_str).map(|s| s == "Running"))
            .unwrap_or(false),
        _ => false,
    }
}

fn tailscale_status_json(socket_args: &[&str]) -> Option<Value> {
    let bin = get_tailscale_bin()?;
    let mut args: Vec<&str> = socket_args.to_vec();
    args.extend(["status", "--json"]);
    let out = Command::new(&bin).args(&args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

struct ProbeCache {
    logged_in: bool,
    logged_in_at: u64,
    running: bool,
    running_at: u64,
}

static PROBES: std::sync::LazyLock<parking_lot::Mutex<ProbeCache>> = std::sync::LazyLock::new(|| {
    parking_lot::Mutex::new(ProbeCache { logged_in: false, logged_in_at: 0, running: false, running_at: 0 })
});

fn custom_socket_args() -> Vec<String> {
    vec!["--socket".into(), tailscale_socket().to_string_lossy().into_owned()]
}

/// `isTailscaleLoggedIn()` — cached, non-blocking, refreshed in the background.
fn is_tailscale_logged_in() -> bool {
    let (logged_in, fresh) = {
        let cache = PROBES.lock();
        (cache.logged_in, now_ms().saturating_sub(cache.logged_in_at) <= PROBE_TTL_MS)
    };
    if !fresh {
        let args = custom_socket_args();
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let value = tailscale_status_json(&arg_refs).or_else(|| {
            tailscale_status_json(&["--socket", "/var/run/tailscale/tailscaled.sock"])
        });
        let result = value
            .map(|j| {
                j.get("BackendState").and_then(Value::as_str) == Some("Running")
                    && j.get("Self").and_then(|s| s.get("Online")).and_then(Value::as_bool) == Some(true)
            })
            .unwrap_or(false);
        let mut cache = PROBES.lock();
        cache.logged_in = result;
        cache.logged_in_at = now_ms();
        return result;
    }
    logged_in
}

/// `isTailscaleLoggedInStrict()` — authoritative, awaitable probe.
fn is_tailscale_logged_in_strict() -> bool {
    let args = custom_socket_args();
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = tailscale_status_json(&arg_refs)
        .map(|j| {
            j.get("BackendState").and_then(Value::as_str) == Some("Running")
                && j.get("Self").and_then(|s| s.get("Online")).and_then(Value::as_bool) == Some(true)
        })
        .unwrap_or(false);
    let mut cache = PROBES.lock();
    cache.logged_in = result;
    cache.logged_in_at = now_ms();
    result
}

/// `isTailscaleRunning()` — any host present in `funnel status --json`'s AllowFunnel.
fn is_tailscale_running() -> bool {
    let (running, fresh) = {
        let cache = PROBES.lock();
        (cache.running, now_ms().saturating_sub(cache.running_at) <= PROBE_TTL_MS)
    };
    if !fresh {
        let args = custom_socket_args();
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let result = funnel_allowed(&arg_refs);
        let mut cache = PROBES.lock();
        cache.running = result;
        cache.running_at = now_ms();
        return result;
    }
    running
}

fn is_tailscale_running_strict() -> bool {
    let args = custom_socket_args();
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = funnel_allowed(&arg_refs);
    let mut cache = PROBES.lock();
    cache.running = result;
    cache.running_at = now_ms();
    result
}

fn funnel_allowed(socket_args: &[&str]) -> bool {
    let Some(bin) = get_tailscale_bin() else { return false };
    let mut args: Vec<&str> = socket_args.to_vec();
    args.extend(["funnel", "status", "--json"]);
    let out = Command::new(&bin).args(&args).output();
    match out {
        Ok(out) if out.status.success() => serde_json::from_slice::<Value>(&out.stdout)
            .ok()
            .and_then(|j| j.get("AllowFunnel").and_then(Value::as_object).map(|m| !m.is_empty()))
            .unwrap_or(false),
        _ => false,
    }
}

/// `getActualFunnelUrl()`: `https://Self.DNSName` (trailing dot stripped).
fn actual_funnel_url() -> Option<String> {
    let args = custom_socket_args();
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let j = tailscale_status_json(&arg_refs)?;
    let dns = j.get("Self").and_then(|s| s.get("DNSName")).and_then(Value::as_str)?;
    let trimmed = dns.trim_end_matches('.');
    if trimmed.is_empty() {
        None
    } else {
        Some(format!("https://{trimmed}"))
    }
}

/// `isCustomDaemonRunning()`: custom socket answers, else `pgrep -x tailscaled`.
fn is_custom_daemon_running() -> bool {
    if get_tailscale_bin().is_none() {
        return false;
    }
    let args = custom_socket_args();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    if tailscale_status_json(&arg_refs).is_some() {
        return true;
    }
    Command::new("pgrep")
        .args(["-x", "tailscaled"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `isDaemonTunMode()`: `pgrep -af "tailscaled.*<socket>"`, `None` when absent.
fn daemon_tun_mode() -> Option<bool> {
    let socket = tailscale_socket().to_string_lossy().into_owned();
    let out = Command::new("pgrep")
        .args(["-af", &format!("tailscaled.*{socket}")])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.trim();
    if line.is_empty() {
        return None;
    }
    Some(!line.contains("--tun=userspace-networking"))
}

/// `isDaemonAlive()`: a tailscaled process exists for our socket, regardless of
/// funnel state (mirrors the cloudflared PID check).
fn is_daemon_alive() -> bool {
    daemon_tun_mode().is_some()
}

fn has_brew() -> bool {
    Command::new("which")
        .arg("brew")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `startDaemonWithPassword(password)` for the non-Windows paths.
fn start_daemon(password: &str) {
    if cfg!(windows) {
        let _ = Command::new("net").args(["start", "Tailscale"]).stdout(Stdio::null()).status();
        return;
    }
    let socket = tailscale_socket().to_string_lossy().into_owned();
    let statedir = tailscale_dir().to_string_lossy().into_owned();
    let want_tun = !password.is_empty();

    // Reuse a daemon already running in the requested mode.
    let status_args = custom_socket_args();
    let arg_refs: Vec<&str> = status_args.iter().map(|s| s.as_str()).collect();
    if tailscale_status_json(&arg_refs).is_some() && want_tun {
        return;
    }

    let _ = Command::new("pkill")
        .args(["-9", "-f", &format!("tailscaled.*{socket}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    std::thread::sleep(Duration::from_millis(1500));

    let daemon = if cfg!(target_os = "macos") { "/usr/local/bin/tailscaled" } else { "tailscaled" };
    let mut args = vec![format!("--socket={socket}"), format!("--statedir={statedir}")];
    if !want_tun {
        args.push("--tun=userspace-networking".into());
    }

    if want_tun {
        // TUN mode needs root: `sudo -S` with the password on stdin.
        use std::io::Write;
        if let Ok(mut child) = Command::new("sudo")
            .args(["-S", daemon])
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(format!("{password}\n").as_bytes());
            }
        }
    } else {
        let _ = Command::new(daemon)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    std::thread::sleep(Duration::from_millis(3000));
}

/// `startLogin(hostname)`: `tailscale up --accept-routes [--hostname=…]`,
/// polling `status --json` for the auth URL.
async fn start_login(hostname: &str) -> Value {
    let Some(bin) = get_tailscale_bin() else {
        return json!({ "error": "Tailscale is not installed. Run the Tailscale installer first." });
    };
    let mut args = custom_socket_args();
    args.push("up".into());
    args.push("--accept-routes".into());
    if !hostname.is_empty() {
        args.push(format!("--hostname={hostname}"));
    }

    let mut child = match tokio::process::Command::new(&bin)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return json!({ "error": e.to_string() }),
    };

    let auth_re = regex::Regex::new(r"https://login\.tailscale\.com/a/[a-zA-Z0-9]+").expect("static regex");
    let mut reader = tokio::io::BufReader::new(child.stderr.take().expect("piped stderr"));
    use tokio::io::AsyncBufReadExt;
    let mut buf = String::new();
    let deadline = tokio::time::sleep(Duration::from_millis(15_000));
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = &mut deadline => break,
            line = reader.read_line(&mut buf) => {
                if let Ok(0) = line { break }
                if let Some(m) = auth_re.find(&buf) {
                    return json!({ "authUrl": m.as_str() });
                }
                buf.clear();
            }
        }
    }
    if is_tailscale_logged_in() {
        json!({ "alreadyLoggedIn": true })
    } else {
        json!({ "error": "tailscale up timed out without auth URL" })
    }
}

/// `startFunnel(port)`.
async fn start_funnel(port: u16) -> Result<Value, String> {
    let Some(bin) = get_tailscale_bin() else { return Err("Tailscale not installed".into()) };

    let mut reset = custom_socket_args();
    reset.extend(["funnel".into(), "--bg".into(), "reset".into()]);
    let _ = Command::new(&bin).args(&reset).stdout(Stdio::null()).stderr(Stdio::null()).status();

    let mut args = custom_socket_args();
    args.extend(["funnel".into(), "--bg".into(), port.to_string()]);
    let out = tokio::time::timeout(
        Duration::from_millis(30_000),
        tokio::process::Command::new(&bin).args(&args).output(),
    )
    .await;

    let (stdout, stderr) = match out {
        Ok(Ok(o)) => (
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        ),
        Ok(Err(e)) => return Err(e.to_string()),
        Err(_) => (String::new(), String::new()),
    };
    let combined = format!("{stdout}{stderr}");

    if combined.contains("Funnel is not enabled") {
        let enable_re = regex::Regex::new(r"https://login\.tailscale\.com/\S+").expect("static regex");
        if let Some(m) = enable_re.find(&combined) {
            return Ok(json!({ "funnelNotEnabled": true, "enableUrl": m.as_str() }));
        }
    }
    match actual_funnel_url() {
        Some(url) => Ok(json!({ "tunnelUrl": url })),
        None => Err(format!("tailscale funnel failed: {}", tail(&combined, 200))),
    }
}

/// `provisionCert(hostname)` — best effort.
async fn provision_cert(hostname: &str) {
    let Some(bin) = get_tailscale_bin() else { return };
    if hostname.is_empty() {
        return;
    }
    let certs = tailscale_dir().join("certs");
    let _ = std::fs::create_dir_all(&certs);
    let cert_file = certs.join(format!("{hostname}.crt"));
    let key_file = certs.join(format!("{hostname}.key"));
    let mut args = custom_socket_args();
    args.extend([
        "cert".into(),
        "--cert-file".into(),
        cert_file.to_string_lossy().into_owned(),
        "--key-file".into(),
        key_file.to_string_lossy().into_owned(),
        hostname.to_string(),
    ]);
    let _ = tokio::time::timeout(
        Duration::from_millis(30_000),
        tokio::process::Command::new(&bin).args(&args).output(),
    )
    .await;
}

fn stop_funnel() {
    let Some(bin) = get_tailscale_bin() else { return };
    let mut args = custom_socket_args();
    args.extend(["funnel".into(), "--bg".into(), "reset".into()]);
    let _ = Command::new(&bin).args(&args).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// `enableTailscale(localPort)`.
async fn enable_tailscale(ext: &Option<Extension<Db>>, local_port: u16) -> Result<Value, TunnelError> {
    // Never fake success when the CLI is missing: the dashboard needs to offer
    // the installer instead (`installTailscale` in the JS build).
    if !is_tailscale_installed() {
        return Err(TunnelError::new(
            StatusCode::BAD_REQUEST,
            "Tailscale is not installed. Run the Tailscale installer first.",
        ));
    }
    let password = crate::mitm::manager::cached_password().unwrap_or_default();
    start_daemon(&password);

    let short_id = load_state()
        .and_then(|s| s.get("shortId").and_then(Value::as_str).map(String::from))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(generate_short_id);

    if !is_tailscale_logged_in_strict() {
        let login = start_login(&short_id).await;
        if let Some(url) = login.get("authUrl").and_then(Value::as_str) {
            return Ok(json!({ "success": false, "needsLogin": true, "authUrl": url }));
        }
    }

    stop_funnel();
    let result = match start_funnel(local_port).await {
        Ok(r) => r,
        Err(e) => {
            if regex::Regex::new("NoState|unexpected state|not logged in|Logged ?out|NeedsLogin")
                .map(|re| re.is_match(&e))
                .unwrap_or(false)
            {
                let login = start_login(&short_id).await;
                if let Some(url) = login.get("authUrl").and_then(Value::as_str) {
                    return Ok(json!({ "success": false, "needsLogin": true, "authUrl": url }));
                }
            }
            return Err(TunnelError::new(StatusCode::INTERNAL_SERVER_ERROR, e));
        }
    };

    if result.get("funnelNotEnabled").and_then(Value::as_bool) == Some(true) {
        return Ok(json!({
            "success": false,
            "funnelNotEnabled": true,
            "enableUrl": result.get("enableUrl").cloned().unwrap_or(Value::Null),
        }));
    }

    if !is_tailscale_logged_in_strict() || !is_tailscale_running_strict() {
        stop_funnel();
        return Ok(json!({
            "success": false,
            "error": "Tailscale not connected. Device may have been removed. Please re-login.",
        }));
    }

    let tunnel_url = result.get("tunnelUrl").and_then(Value::as_str).unwrap_or("").to_string();
    write_settings(ext, json!({ "tailscaleEnabled": true, "tailscaleUrl": tunnel_url }));

    if let Ok(parsed) = url::Url::parse(&tunnel_url) {
        if let Some(host) = parsed.host_str() {
            provision_cert(host).await;
        }
    }

    let cancel = std::sync::atomic::AtomicBool::new(false);
    // Health timeouts are non-fatal: DNS for *.ts.net may still be propagating.
    if let Err(e) = wait_for_health(&tunnel_url, &cancel, TAILSCALE_ENABLE_TIMEOUT_MS).await {
        if !e.starts_with("Health check timeout") {
            return Err(TunnelError::new(StatusCode::INTERNAL_SERVER_ERROR, e));
        }
        tracing::warn!("[Tailscale] health check timed out, will retry via watchdog");
    }

    Ok(json!({ "success": true, "tunnelUrl": tunnel_url }))
}

fn disable_tailscale(ext: &Option<Extension<Db>>) -> Value {
    stop_funnel();
    write_settings(ext, json!({ "tailscaleEnabled": false, "tailscaleUrl": "" }));
    json!({ "success": true })
}

fn tailscale_status(ext: &Option<Extension<Db>>) -> Value {
    let settings = settings_of(ext);
    let settings_enabled = settings.get("tailscaleEnabled").and_then(Value::as_bool) == Some(true);
    let tunnel_url = settings.get("tailscaleUrl").and_then(Value::as_str).unwrap_or("").to_string();
    let logged_in = if settings_enabled { is_tailscale_logged_in() } else { false };
    let running = if logged_in { is_tailscale_running() } else { false };
    json!({
        "enabled": settings_enabled && running,
        "settingsEnabled": settings_enabled,
        "tunnelUrl": tunnel_url,
        "running": running,
        "loggedIn": logged_in,
    })
}

// ─── watchdog + network monitor (initializeApp.js) ──────────────────────────
//
// Both managers self-reconnect (cloudflared `--retries 99`, tailscaled is an OS
// daemon), so the supervisor only intervenes when the process is gone.

struct MonitoringState {
    /// Spawned watchdog task; dropping the sender stops it.
    watchdog: Option<tokio::task::JoinHandle<()>>,
    monitor: Option<tokio::task::JoinHandle<()>>,
    /// `g.lastNetworkFingerprint`.
    last_fingerprint: Option<String>,
    /// `g.lastOnline`: `None` until the first reachability probe.
    last_online: Option<bool>,
    /// `g.lastWatchdogTick`.
    last_tick_ms: u64,
}

static MONITOR: std::sync::LazyLock<parking_lot::Mutex<MonitoringState>> =
    std::sync::LazyLock::new(|| {
        parking_lot::Mutex::new(MonitoringState {
            watchdog: None,
            monitor: None,
            last_fingerprint: None,
            last_online: None,
            last_tick_ms: 0,
        })
    });

/// `VIRTUAL_IFACE_REGEX`: skip interfaces that flap and cause false netchange.
fn is_virtual_iface(name: &str) -> bool {
    const PREFIXES: [&str; 13] = [
        "utun", "awdl", "llw", "anpi", "bridge", "gif", "stf", "ipsec", "ap", "tun", "tap", "vmnet",
        "veth",
    ];
    let lower = name.to_ascii_lowercase();
    PREFIXES.iter().any(|p| lower.starts_with(p)) || lower.starts_with("docker")
}

/// `getNetworkFingerprint()`: sorted `iface:ipv4` of non-internal interfaces.
fn network_fingerprint() -> String {
    let mut entries: Vec<String> = match if_addrs_v4() {
        Some(list) => list
            .into_iter()
            .filter(|(name, _)| !is_virtual_iface(name))
            .map(|(name, ip)| format!("{name}:{ip}"))
            .collect(),
        None => Vec::new(),
    };
    entries.sort();
    entries.join("|")
}

/// IPv4 addresses per interface. Reads `/sys/class/net` on Linux; elsewhere the
/// monitor degrades to "no fingerprint" and only the online edge triggers a
/// restart. (A netlink/getifaddrs dependency is not worth it for a supervisor.)
fn if_addrs_v4() -> Option<Vec<(String, String)>> {
    let mut out = Vec::new();
    let entries = std::fs::read_dir("/sys/class/net").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Linux exposes one directory per assigned address under the interface.
        let Ok(addrs) = std::fs::read_dir(entry.path()) else { continue };
        for addr in addrs.flatten() {
            let ip = addr.file_name().to_string_lossy().into_owned();
            if ip.parse::<std::net::Ipv4Addr>().is_ok() {
                out.push((name.clone(), ip));
            }
        }
    }
    Some(out)
}

/// `safeRestartTunnel(reason)`.
async fn safe_restart_tunnel(reason: &str) {
    let ext = db_extension();
    let settings = settings_of(&ext);
    if settings.get("tunnelEnabled").and_then(Value::as_bool) != Some(true) {
        return;
    }
    if CANCEL.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    if SPAWN_IN_PROGRESS.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }

    // A live process is trusted: cloudflared self-reconnects and keeps its URL.
    if is_cloudflared_running() {
        return;
    }

    let force = is_force_restart_reason(reason);
    let last = LAST_RESTART_AT.load(std::sync::atomic::Ordering::SeqCst);
    if !force && now_ms().saturating_sub(last) < RESTART_COOLDOWN_MS {
        tracing::info!("[Tunnel] degraded but cooldown active, skip ({reason})");
        return;
    }
    if !check_internet().await {
        return;
    }

    tracing::info!("[Tunnel] safeRestart ({reason}) — tunnel unreachable");
    match enable_tunnel(&ext, active_local_port().unwrap_or(20128)).await {
        Ok(_) => {
            LAST_RESTART_AT.store(now_ms(), std::sync::atomic::Ordering::SeqCst);
            tracing::info!("[Tunnel] restart success");
        }
        Err(e) => {
            if !e.message.contains("cloudflared killed") && !e.message.contains("tunnel cancelled") {
                tracing::warn!("[Tunnel] restart failed: {}", e.message);
            }
        }
    }
}

/// `safeRestartTailscale(reason)`.
async fn safe_restart_tailscale(reason: &str) {
    let ext = db_extension();
    let settings = settings_of(&ext);
    if settings.get("tailscaleEnabled").and_then(Value::as_bool) != Some(true) {
        return;
    }
    if CANCEL.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    if SPAWN_IN_PROGRESS.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }

    let running = if reason == "startup" {
        is_tailscale_running_strict()
    } else {
        is_tailscale_running()
    };
    if running {
        return;
    }

    // Daemon alive but funnel dropped: recover the funnel only, never restart
    // the daemon (that would drop the login).
    if is_daemon_alive() {
        if let Some(port) = active_local_port() {
            match start_funnel(port).await {
                Ok(_) => {
                    LAST_RESTART_AT.store(now_ms(), std::sync::atomic::Ordering::SeqCst);
                    tracing::info!("[Tailscale] funnel re-established (daemon alive)");
                }
                Err(e) => tracing::warn!("[Tailscale] funnel recovery failed: {e}"),
            }
            return;
        }
    }

    let force = is_force_restart_reason(reason);
    let last = LAST_RESTART_AT.load(std::sync::atomic::Ordering::SeqCst);
    if !force && now_ms().saturating_sub(last) < RESTART_COOLDOWN_MS {
        tracing::info!("[Tailscale] degraded but cooldown active, skip ({reason})");
        return;
    }
    if !check_internet().await {
        return;
    }

    tracing::info!("[Tailscale] safeRestart ({reason}) — daemon not running");
    match enable_tailscale(&ext, active_local_port().unwrap_or(20128)).await {
        Ok(_) => {
            LAST_RESTART_AT.store(now_ms(), std::sync::atomic::Ordering::SeqCst);
            tracing::info!("[Tailscale] restart success");
        }
        Err(e) => tracing::warn!("[Tailscale] restart failed: {}", e.message),
    }
}

/// A DB handle for background work (`None` when the database cannot be opened).
fn db_extension() -> Option<Extension<Db>> {
    get_db(None).ok().map(Extension)
}

fn stop_watchdog() {
    if let Some(handle) = MONITOR.lock().watchdog.take() {
        handle.abort();
    }
}

fn stop_network_monitor() {
    let mut monitor = MONITOR.lock();
    if let Some(handle) = monitor.monitor.take() {
        handle.abort();
    }
    monitor.last_fingerprint = None;
    monitor.last_online = None;
}

fn start_watchdog() {
    let mut state = MONITOR.lock();
    if state.watchdog.is_some() {
        return;
    }
    state.watchdog = Some(tokio::spawn(async {
        let mut ticker = tokio::time::interval(Duration::from_millis(WATCHDOG_INTERVAL_MS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick is immediate; skip it so startup is not restarted twice.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            safe_restart_tunnel("watchdog").await;
            safe_restart_tailscale("watchdog").await;
        }
    }));
}

fn start_network_monitor() {
    let mut state = MONITOR.lock();
    if state.monitor.is_some() {
        return;
    }
    state.last_fingerprint = Some(network_fingerprint());
    state.last_tick_ms = now_ms();
    state.last_online = None;
    state.monitor = Some(tokio::spawn(async {
        let mut ticker = tokio::time::interval(Duration::from_millis(NETWORK_CHECK_INTERVAL_MS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let now = now_ms();
            let (elapsed, previous_fp) = {
                let mut monitor = MONITOR.lock();
                let elapsed = now.saturating_sub(monitor.last_tick_ms);
                monitor.last_tick_ms = now;
                (elapsed, monitor.last_fingerprint.clone())
            };

            let current = network_fingerprint();
            let network_changed = previous_fp.as_deref() != Some(current.as_str());
            let was_sleep = elapsed > NETWORK_CHECK_INTERVAL_MS * 6;
            if network_changed {
                MONITOR.lock().last_fingerprint = Some(current);
            }

            let online = check_internet().await;
            let was_offline = MONITOR.lock().last_online == Some(false);
            MONITOR.lock().last_online = Some(online);
            if !online {
                continue;
            }

            let online_edge = was_offline;
            if !network_changed && !was_sleep && !online_edge {
                continue;
            }

            // Let DHCP/DNS settle before probing.
            tokio::time::sleep(Duration::from_millis(NETWORK_SETTLE_MS)).await;

            let reason = if online_edge {
                "online"
            } else if was_sleep && network_changed {
                "sleep+netchange"
            } else if was_sleep {
                "sleep"
            } else {
                "netchange"
            };
            safe_restart_tunnel(reason).await;
            safe_restart_tailscale(reason).await;
        }
    }));
}

/// `configureTunnelMonitoring(settings)`: run the supervisor only while at
/// least one tunnel type is enabled in settings.
pub fn configure_tunnel_monitoring(settings: &Value) {
    let enabled = settings.get("tunnelEnabled").and_then(Value::as_bool) == Some(true)
        || settings.get("tailscaleEnabled").and_then(Value::as_bool) == Some(true);
    if enabled {
        start_watchdog();
        start_network_monitor();
    } else {
        stop_watchdog();
        stop_network_monitor();
    }
}

/// Startup entry point: read settings and arm the supervisor. Called from
/// `app()` so an already-enabled tunnel recovers after a restart.
pub fn init_monitoring() {
    let settings = match get_db(None) {
        Ok(db) => settings_of(&Some(Extension(db))),
        Err(_) => return,
    };
    configure_tunnel_monitoring(&settings);
}

// ─── status cache ───────────────────────────────────────────────────────────

struct StatusCache {
    tunnel: Option<Value>,
    tailscale: Option<Value>,
    fetched_at: u64,
}

static STATUS_CACHE: std::sync::LazyLock<parking_lot::Mutex<StatusCache>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(StatusCache { tunnel: None, tailscale: None, fetched_at: 0 }));

fn cached_probes(ext: &Option<Extension<Db>>) -> (Value, Value) {
    {
        let cache = STATUS_CACHE.lock();
        if let (Some(tunnel), Some(tailscale)) = (&cache.tunnel, &cache.tailscale) {
            if now_ms().saturating_sub(cache.fetched_at) < STATUS_CACHE_TTL_MS {
                return (tunnel.clone(), tailscale.clone());
            }
        }
    }
    let tunnel = tunnel_status(ext);
    let tailscale = tailscale_status(ext);
    let mut cache = STATUS_CACHE.lock();
    cache.tunnel = Some(tunnel.clone());
    cache.tailscale = Some(tailscale.clone());
    cache.fetched_at = now_ms();
    (tunnel, tailscale)
}

// ─── Handlers ───────────────────────────────────────────────────────────────

/// GET /api/tunnel/status — `{ tunnel, tailscale, download }`.
pub async fn get_status(ext: Option<Extension<Db>>) -> Response {
    let (tunnel, tailscale) = cached_probes(&ext);
    Json(json!({
        "tunnel": tunnel,
        "tailscale": tailscale,
        "download": download_status(),
    }))
    .into_response()
}

/// POST /api/tunnel/enable
pub async fn post_enable(ext: Option<Extension<Db>>) -> Response {
    match enable_tunnel(&ext, 20128).await {
        Ok(result) => {
            // DNS warmup so the Cloudflare edge has the route before the UI probes it.
            tokio::time::sleep(Duration::from_millis(DNS_WARMUP_DELAY_MS)).await;
            refresh_monitoring(&ext);
            Json(result).into_response()
        }
        Err(e) => (e.status, Json(json!({ "error": e.message }))).into_response(),
    }
}

/// POST /api/tunnel/disable
pub async fn post_disable(ext: Option<Extension<Db>>) -> Response {
    let result = disable_tunnel(&ext);
    refresh_monitoring(&ext);
    Json(result).into_response()
}

/// POST /api/tunnel/tailscale-enable
pub async fn post_tailscale_enable(ext: Option<Extension<Db>>) -> Response {
    match enable_tailscale(&ext, 20128).await {
        Ok(result) => {
            refresh_monitoring(&ext);
            Json(result).into_response()
        }
        Err(e) => (e.status, Json(json!({ "error": e.message }))).into_response(),
    }
}

/// POST /api/tunnel/tailscale-disable
pub async fn post_tailscale_disable(ext: Option<Extension<Db>>) -> Response {
    let result = disable_tailscale(&ext);
    refresh_monitoring(&ext);
    Json(result).into_response()
}

/// Re-read settings and re-arm the supervisor, like the JS routes'
/// `getSettings().then(configureTunnelMonitoring)`.
fn refresh_monitoring(ext: &Option<Extension<Db>>) {
    configure_tunnel_monitoring(&settings_of(ext));
}

/// GET /api/tunnel/tailscale-check
pub async fn get_tailscale_check() -> Response {
    let installed = is_tailscale_installed();
    let platform = std::env::consts::OS;
    let platform = if platform == "macos" { "darwin" } else { platform };
    let brew_available = platform == "darwin" && has_brew();
    let custom_daemon_running = if installed { is_custom_daemon_running() } else { false };
    let system_daemon_running = if installed { is_system_daemon_running() } else { false };
    let daemon_running = custom_daemon_running || system_daemon_running;
    let logged_in = if daemon_running { is_tailscale_logged_in() } else { false };
    Json(json!({
        "installed": installed,
        "loggedIn": logged_in,
        "platform": platform,
        "brewAvailable": brew_available,
        "daemonRunning": daemon_running,
        "customDaemonRunning": custom_daemon_running,
        "systemDaemonRunning": system_daemon_running,
        "hasCachedPassword": crate::mitm::manager::cached_password().is_some(),
    }))
    .into_response()
}

/// POST /api/tunnel/tailscale-install — SSE progress stream.
pub async fn post_tailscale_install(body: Option<Json<Value>>) -> Response {
    let platform = std::env::consts::OS;
    let platform = if platform == "macos" { "darwin" } else { platform };
    let is_windows = cfg!(windows);
    let is_brew = platform == "darwin" && has_brew();
    let needs_password = !is_windows && !is_brew;

    let supplied = body
        .as_ref()
        .and_then(|Json(v)| v.get("sudoPassword").and_then(Value::as_str))
        .map(String::from)
        .filter(|s| !s.is_empty())
        .or_else(crate::mitm::manager::cached_password)
        .unwrap_or_default();

    if needs_password && supplied.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Sudo password is required" })),
        )
            .into_response();
    }

    let short_id = load_state()
        .and_then(|s| s.get("shortId").and_then(Value::as_str).map(String::from))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(generate_short_id);

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(64);
    tokio::spawn(async move {
        let send = |event: &str, data: Value| Event::default().event(event).data(data.to_string());
        let progress = |msg: &str| send("progress", json!({ "message": msg }));

        let _ = tx.send(Ok(progress("Installing Tailscale..."))).await;
        match install_tailscale(platform, &supplied).await {
            Ok(()) => {
                let _ = tx.send(Ok(progress("Starting daemon..."))).await;
                start_daemon(&supplied);
                let _ = tx.send(Ok(progress("Logging in..."))).await;
                let login = start_login(&short_id).await;
                let auth_url = login.get("authUrl").cloned().unwrap_or(Value::Null);
                let _ = tx.send(Ok(send("done", json!({ "success": true, "authUrl": auth_url })))).await;
            }
            Err(e) => {
                let message = if e.contains("incorrect password") || e.contains("Sorry") {
                    "Wrong sudo password".to_string()
                } else {
                    e
                };
                let _ = tx.send(Ok(send("error", json!({ "error": message })))).await;
            }
        }
    });

    let stream = tokio_stream_wrapper(rx);
    let mut resp = Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(25)))
        .into_response();
    let headers = resp.headers_mut();
    headers.insert(axum::http::header::CACHE_CONTROL, axum::http::HeaderValue::from_static("no-cache"));
    headers.insert(axum::http::header::CONNECTION, axum::http::HeaderValue::from_static("keep-alive"));
    resp
}

fn tokio_stream_wrapper(
    rx: tokio::sync::mpsc::Receiver<Result<Event, std::convert::Infallible>>,
) -> impl futures::Stream<Item = Result<Event, std::convert::Infallible>> {
    async_stream::stream! {
        let mut rx = rx;
        while let Some(item) = rx.recv().await {
            yield item;
        }
    }
}

/// `installTailscale(password, hostname, onProgress)` — platform installer.
async fn install_tailscale(platform: &str, password: &str) -> Result<(), String> {
    if cfg!(windows) {
        // shortcut: Windows installs via the MSI with UAC; the Rust port shells
        // out to msiexec and does not raise an interactive UAC prompt.
        let msi = std::env::temp_dir().join("tailscale-setup.msi");
        let status = Command::new("curl.exe")
            .args([
                "-L",
                "-o",
                &msi.to_string_lossy(),
                "https://pkgs.tailscale.com/stable/tailscale-setup-latest-amd64.msi",
            ])
            .status()
            .map_err(|e| format!("Download failed: {e}"))?;
        if !status.success() {
            return Err("Download failed".into());
        }
        let status = Command::new("msiexec")
            .args(["/i", &msi.to_string_lossy(), "TS_NOLAUNCH=true", "/quiet", "/norestart"])
            .status()
            .map_err(|e| format!("msiexec failed: {e}"))?;
        let _ = std::fs::remove_file(&msi);
        if !status.success() {
            return Err(format!("msiexec failed (code {})", status.code().unwrap_or(-1)));
        }
        return Ok(());
    }

    if platform == "darwin" && has_brew() {
        let status = Command::new("brew")
            .args(["install", "tailscale"])
            .status()
            .map_err(|e| format!("brew install failed: {e}"))?;
        if !status.success() {
            return Err(format!("brew install failed (code {})", status.code().unwrap_or(-1)));
        }
        return Ok(());
    }

    // Reject newlines: the password is written to sudo's stdin verbatim.
    if password.contains('\n') {
        return Err("Invalid sudo password".into());
    }

    if platform == "darwin" {
        // No brew: download the .pkg and install with `sudo installer`.
        let pkg = std::env::temp_dir().join("tailscale.pkg");
        let status = Command::new("curl")
            .args(["-fL", "https://pkgs.tailscale.com/stable/tailscale-latest.pkg", "-o", &pkg.to_string_lossy()])
            .status()
            .map_err(|e| format!("Download failed: {e}"))?;
        if !status.success() {
            return Err("Download failed".into());
        }
        let result = run_sudo(
            password,
            &format!("installer -pkg '{}' -target /", pkg.to_string_lossy()),
        )
        .await;
        let _ = std::fs::remove_file(&pkg);
        return result.map(|_| ());
    }

    // Linux: fetch install.sh, run it by path so `sh` never reads attacker stdin.
    let script_body = {
        let out = Command::new("curl")
            .args(["-fsSL", "https://tailscale.com/install.sh"])
            .output()
            .map_err(|e| format!("Failed to download install script: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "Failed to download install script: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        out.stdout
    };
    let script_path = std::env::temp_dir().join(format!("tailscale-install-{}.sh", uuid::Uuid::new_v4()));
    std::fs::write(&script_path, &script_body).map_err(|e| format!("Failed to write install script: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700));
    }
    let result = run_sudo(password, &format!("sh '{}'", script_path.to_string_lossy())).await;
    let _ = std::fs::remove_file(&script_path);
    result.map(|_| ())
}

/// `sudo -S sh -c <script>` with the password on stdin; stderr surfaces as the error.
async fn run_sudo(password: &str, script: &str) -> Result<String, String> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new("sudo")
        .args(["-S", "sh", "-c", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(format!("{password}\n").as_bytes()).await;
        let _ = stdin.shutdown().await;
    }
    let out = child.wait_with_output().await.map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if stderr.contains("incorrect password") || stderr.contains("Sorry") {
        Err("Wrong sudo password".into())
    } else if stderr.trim().is_empty() {
        Err(format!("Exit code {}", out.status.code().unwrap_or(-1)))
    } else {
        Err(stderr)
    }
}

pub fn routes() -> Router {
    Router::new()
        .route("/api/tunnel/status", get(get_status))
        .route("/api/tunnel/enable", post(post_enable))
        .route("/api/tunnel/disable", post(post_disable))
        .route("/api/tunnel/tailscale-check", get(get_tailscale_check))
        .route("/api/tunnel/tailscale-enable", post(post_tailscale_enable))
        .route("/api/tunnel/tailscale-disable", post(post_tailscale_disable))
        .route("/api/tunnel/tailscale-install", post(post_tailscale_install))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_id_is_six_safe_chars() {
        let id = generate_short_id();
        assert_eq!(id.len(), 6);
        assert!(id.chars().all(|c| "abcdefghijklmnpqrstuvwxyz23456789".contains(c)), "{id}");
    }

    #[test]
    fn quick_tunnel_url_skips_api_host() {
        let line = "INF https://api.trycloudflare.com then https://blue-fox-abc.trycloudflare.com";
        assert_eq!(
            quick_tunnel_url(line).as_deref(),
            Some("https://blue-fox-abc.trycloudflare.com")
        );
        assert_eq!(quick_tunnel_url("no url here"), None);
    }

    #[test]
    fn public_url_is_empty_without_short_id() {
        assert_eq!(public_url(""), "");
        assert_eq!(public_url("abc234"), "https://rabc234.abc-tunnel.us");
    }

    #[test]
    fn dns_query_encodes_labels() {
        let packet = build_dns_query("a.example.com", 0xbeef);
        assert_eq!(&packet[0..2], &[0xbe, 0xef]);
        assert_eq!(&packet[2..4], &[0x01, 0x00], "recursion desired");
        assert_eq!(&packet[4..6], &[0x00, 0x01], "one question");
        assert_eq!(packet[12], 1);
        assert_eq!(&packet[13..14], b"a");
        // QTYPE A + QCLASS IN at the tail.
        let tail = &packet[packet.len() - 4..];
        assert_eq!(tail, &[0x00, 0x01, 0x00, 0x01]);
    }

    #[test]
    fn dns_reply_requires_matching_id_and_answers() {
        let mut reply = vec![0u8; 12];
        reply[0..2].copy_from_slice(&7u16.to_be_bytes());
        reply[3] = 0; // rcode NOERROR
        reply[6..8].copy_from_slice(&1u16.to_be_bytes()); // ANCOUNT = 1
        assert!(dns_reply_resolved(&reply, 7));
        assert!(!dns_reply_resolved(&reply, 8), "wrong transaction id");

        reply[6..8].copy_from_slice(&0u16.to_be_bytes());
        assert!(!dns_reply_resolved(&reply, 7), "no answers");

        reply[6..8].copy_from_slice(&1u16.to_be_bytes());
        reply[3] = 3; // NXDOMAIN
        assert!(!dns_reply_resolved(&reply, 7), "NXDOMAIN is not resolved");
        assert!(!dns_reply_resolved(&reply[..8], 7), "short packet");
    }

    #[tokio::test]
    async fn resolve_dns_accepts_literal_ip() {
        assert!(resolve_dns("203.0.113.9", 50).await);
        assert!(!resolve_dns("", 50).await);
    }

    #[test]
    fn virtual_interfaces_are_skipped() {
        for name in ["utun3", "awdl0", "bridge100", "docker0", "veth12a", "tun0", "tap1"] {
            assert!(is_virtual_iface(name), "{name} should be virtual");
        }
        for name in ["eth0", "en0", "wlan0", "enp3s0"] {
            assert!(!is_virtual_iface(name), "{name} is physical");
        }
    }

    #[test]
    fn network_fingerprint_is_stable_and_sorted() {
        let first = network_fingerprint();
        let second = network_fingerprint();
        assert_eq!(first, second, "two immediate reads must agree");
        if first.contains('|') {
            let parts: Vec<&str> = first.split('|').collect();
            let mut sorted = parts.clone();
            sorted.sort();
            assert_eq!(parts, sorted);
        }
    }

    #[test]
    fn force_restart_reasons_match_js() {
        for reason in ["startup", "netchange", "sleep", "sleep+netchange", "online", "unexpected-exit"] {
            assert!(is_force_restart_reason(reason), "{reason} bypasses cooldown");
        }
        assert!(!is_force_restart_reason("watchdog"));
        assert!(!is_force_restart_reason(""));
    }

    // `configureTunnelMonitoring` must be idempotent and must not need a runtime
    // when turning monitoring off.
    #[test]
    fn configure_monitoring_off_is_a_noop() {
        configure_tunnel_monitoring(&json!({}));
        configure_tunnel_monitoring(&json!({ "tunnelEnabled": false, "tailscaleEnabled": false }));
        assert!(MONITOR.lock().watchdog.is_none());
        assert!(MONITOR.lock().monitor.is_none());
    }
}
