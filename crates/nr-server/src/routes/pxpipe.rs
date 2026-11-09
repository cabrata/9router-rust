//! PXPIPE token-saver routes. Faithful port of `src/app/api/pxpipe/**` with
//! `src/lib/pxpipe/{install,loader,service,events}.js`.
//!
//! "Library mode": in the JS build `start`/`stop` load and drop an in-process
//! npm module that rewrites Anthropic requests. The Rust build keeps the same
//! on-disk layout and the same management surface, and shells out to `node`
//! for the module self-test (see the `shortcut:` note on [`node_self_test`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{Extension, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use nr_db::{get_settings, Db};
use parking_lot::Mutex;
use serde_json::{json, Map, Value};

use super::api::common::get_db;

const PXPIPE_PACKAGE: &str = "pxpipe-proxy";
const INSTALL_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MAX_LOG_LINES: usize = 200;
const MAX_EVENTS_FILE_BYTES: u64 = 5 * 1024 * 1024;
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

fn pxpipe_dir() -> PathBuf {
    nr_core::paths::data_dir().join("pxpipe")
}

fn install_log() -> PathBuf {
    pxpipe_dir().join("install.log")
}

fn package_root() -> PathBuf {
    pxpipe_dir().join("node_modules").join(PXPIPE_PACKAGE)
}

fn library_entry() -> PathBuf {
    package_root().join("dist").join("core").join("library.js")
}

fn events_file() -> PathBuf {
    pxpipe_dir().join("events.jsonl")
}

fn rotated_file() -> PathBuf {
    pxpipe_dir().join("events.jsonl.1")
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ─── install.js ─────────────────────────────────────────────────────────────

/// `EXTENDED_PATH` — packaged/launchd environments miss the Node bin dirs.
fn extended_path() -> String {
    let mut parts: Vec<String> = if cfg!(windows) {
        let pf = std::env::var("ProgramFiles").unwrap_or_default();
        let appdata = std::env::var("APPDATA").unwrap_or_default();
        vec![format!("{pf}\\nodejs"), format!("{appdata}\\npm")]
    } else {
        let home = nr_core::paths::home_dir();
        vec![
            "/usr/local/bin".into(),
            "/opt/homebrew/bin".into(),
            home.join(".local/bin").to_string_lossy().into_owned(),
            "/usr/bin".into(),
            "/bin".into(),
        ]
    };
    if let Ok(p) = std::env::var("PATH") {
        parts.push(p);
    }
    parts.retain(|p| !p.is_empty());
    parts.join(if cfg!(windows) { ";" } else { ":" })
}

/// `findNpm()` — first line of `which npm`.
fn find_npm() -> Option<String> {
    let which = if cfg!(windows) { "where" } else { "which" };
    let out = std::process::Command::new(which)
        .arg("npm")
        .env("PATH", extended_path())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().next().map(|l| l.trim().to_string()).filter(|l| !l.is_empty())
}

/// `getInstallInfo()`: `{ installed, version, path }`.
fn install_info() -> Value {
    let pkg_json = package_root().join("package.json");
    if !pkg_json.exists() || !library_entry().exists() {
        return json!({ "installed": false, "version": Value::Null, "path": Value::Null });
    }
    let version = std::fs::read_to_string(&pkg_json)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("version").and_then(Value::as_str).map(String::from));
    json!({
        "installed": true,
        "version": version,
        "path": package_root().to_string_lossy(),
    })
}

static INSTALLING: AtomicBool = AtomicBool::new(false);
static INSTALL_LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

type PxError = (StatusCode, Value);

/// `installPxpipe()` — install (or repair) into `DATA_DIR/pxpipe`.
async fn install_pxpipe() -> Result<Value, PxError> {
    let _guard = INSTALL_LOCK.lock().await;
    let npm = find_npm().ok_or_else(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({
                "error": "npm not found on PATH — Node.js/npm is required to install PXPIPE",
                "code": "NPM_NOT_FOUND",
            }),
        )
    })?;

    let dir = pxpipe_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() })));
    }
    let pkg_json = dir.join("package.json");
    if !pkg_json.exists() {
        let body = serde_json::to_string_pretty(&json!({
            "name": "9router-pxpipe-host",
            "private": true,
        }))
        .unwrap_or_default();
        let _ = std::fs::write(&pkg_json, body);
    }

    let log_path = install_log();
    let log_line = format!(
        "\n[{}] npm install {PXPIPE_PACKAGE}@latest\n",
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    );
    append_file(&log_path, &log_line);

    INSTALLING.store(true, Ordering::SeqCst);
    let result = run_install_command(&npm, &dir, &log_path).await;
    INSTALLING.store(false, Ordering::SeqCst);
    result?;

    let info = install_info();
    if info.get("installed").and_then(Value::as_bool) != Some(true) {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": "install finished but package is missing — see install.log" }),
        ));
    }
    Ok(info)
}

fn append_file(path: &Path, text: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(text.as_bytes());
    }
}

async fn run_install_command(npm: &str, dir: &Path, log_path: &Path) -> Result<(), PxError> {
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() })))?;
    let err_log = log
        .try_clone()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() })))?;

    let child = tokio::process::Command::new(npm)
        .args([
            "install",
            &format!("{PXPIPE_PACKAGE}@latest"),
            "--no-audit",
            "--no-fund",
            "--omit=dev",
        ])
        .current_dir(dir)
        .env("PATH", extended_path())
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(err_log)
        .kill_on_drop(true)
        .spawn();

    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": format!("npm install failed to start: {e}"), "code": Value::Null }),
            ))
        }
    };

    match tokio::time::timeout(INSTALL_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({
                "error": format!("npm install exited with code {} — see install.log", status.code().unwrap_or(-1)),
                "code": Value::Null,
            }),
        )),
        Ok(Err(e)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": e.to_string(), "code": Value::Null }),
        )),
        Err(_) => {
            let _ = child.start_kill();
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": "npm install timed out after 5 minutes — see install.log", "code": Value::Null }),
            ))
        }
    }
}

/// `getInstallLogTail(maxLines)`.
fn install_log_tail(max_lines: usize) -> String {
    let Ok(text) = std::fs::read_to_string(install_log()) else { return String::new() };
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

// ─── loader.js ──────────────────────────────────────────────────────────────

struct LoadedModule {
    version: Option<String>,
    loaded_at: i64,
}

static LOADED: LazyLock<Mutex<Option<LoadedModule>>> = LazyLock::new(|| Mutex::new(None));

/// `unloadPxpipe()` — returns whether a module was loaded.
fn unload_pxpipe() -> bool {
    LOADED.lock().take().is_some()
}

/// `loadPxpipe()` + `selfTest()`: run the installed transform through `node`
/// and report the machine-readable reason.
///
/// shortcut: the JS build imports the module in-process; Rust shells out to
/// node once per load/self-test. Upgrade when a native pxpipe port lands.
async fn node_self_test(entry: &Path) -> Result<String, String> {
    const SCRIPT: &str = r#"
import { pathToFileURL } from "node:url";
const entry = process.env.PXPIPE_ENTRY;
const mod = await import(pathToFileURL(entry).href + "?v=" + Date.now());
if (typeof mod.transformAnthropicMessages !== "function") {
  throw new Error("installed pxpipe package does not export transformAnthropicMessages");
}
const body = new TextEncoder().encode(JSON.stringify({
  model: "claude-fable-5", max_tokens: 16,
  messages: [{ role: "user", content: "ping" }],
}));
const result = await mod.transformAnthropicMessages({ body, model: "claude-fable-5" });
if (!result || typeof result.applied !== "boolean" || !(result.body instanceof Uint8Array)) {
  throw new Error("transform returned an unexpected shape");
}
console.log(JSON.stringify({ reason: result.reason ?? null }));
"#;

    let out = tokio::process::Command::new("node")
        .args(["--input-type=module", "-e", SCRIPT])
        .env("PXPIPE_ENTRY", entry)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("node not available: {e}"))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let msg = stderr.lines().find(|l| l.contains("Error")).unwrap_or(stderr.trim());
        return Err(if msg.is_empty() { "node exited with an error".to_string() } else { msg.to_string() });
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let reason = serde_json::from_str::<Value>(stdout.trim())
        .ok()
        .and_then(|v| v.get("reason").and_then(Value::as_str).map(String::from))
        .unwrap_or_else(|| "unknown".into());
    Ok(reason)
}

/// `loadPxpipe()`: verify the install, self-test, then mark loaded.
async fn load_pxpipe() -> Result<Value, PxError> {
    if let Some(m) = LOADED.lock().as_ref() {
        return Ok(json!({ "loaded": true, "version": m.version, "loadedAt": m.loaded_at }));
    }
    let info = install_info();
    if info.get("installed").and_then(Value::as_bool) != Some(true) {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": "PXPIPE is not installed", "code": "NOT_INSTALLED" }),
        ));
    }
    if let Err(msg) = node_self_test(&library_entry()).await {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": msg, "code": Value::Null })));
    }
    let version = info.get("version").and_then(Value::as_str).map(String::from);
    let loaded_at = now_ms();
    *LOADED.lock() = Some(LoadedModule { version: version.clone(), loaded_at });
    Ok(json!({ "loaded": true, "version": version, "loadedAt": loaded_at }))
}

// ─── service.js ─────────────────────────────────────────────────────────────

/// `getPxpipeStatus()`.
fn pxpipe_status() -> Value {
    let install = install_info();
    let loaded = LOADED.lock();
    let (running, loaded_at) = match loaded.as_ref() {
        Some(m) => (true, m.loaded_at),
        None => (false, 0),
    };
    json!({
        "installed": install.get("installed").cloned().unwrap_or(Value::Bool(false)),
        "installing": INSTALLING.load(Ordering::SeqCst),
        "version": install.get("version").cloned().unwrap_or(Value::Null),
        "path": install.get("path").cloned().unwrap_or(Value::Null),
        "running": running,
        "loadedAt": if running { json!(loaded_at) } else { Value::Null },
        "uptimeMs": if running { json!(now_ms() - loaded_at) } else { json!(0) },
        "npmAvailable": find_npm().is_some(),
        "mode": "library",
    })
}

/// `runHealthCheck()` — installed, then module loads, then test transform.
async fn run_health_check() -> Value {
    let mut checks: Vec<Value> = Vec::new();
    let install = install_info();
    let installed = install.get("installed").and_then(Value::as_bool).unwrap_or(false);
    let version = install.get("version").cloned().unwrap_or(Value::Null);
    checks.push(json!({
        "id": "installed",
        "label": "PXPIPE installed",
        "ok": installed,
        "detail": match version.as_str() { Some(v) => json!(format!("v{v}")), None => Value::Null },
    }));
    if !installed {
        return json!({ "healthy": false, "checks": checks, "error": "pxpipe not installed" });
    }

    let started = now_ms();
    match node_self_test(&library_entry()).await {
        Ok(reason) => {
            let detail = format!("v{}", version.as_str().unwrap_or("0"));
            checks.push(json!({ "id": "module", "label": "Transform module loads", "ok": true, "detail": detail }));
            let transform = format!("{}ms ({reason})", now_ms() - started);
            checks.push(json!({ "id": "transform", "label": "Test request transforms", "ok": true, "detail": transform }));
            json!({ "healthy": true, "checks": checks, "error": Value::Null })
        }
        Err(msg) => {
            checks.push(json!({ "id": "module", "label": "Transform module loads", "ok": false, "detail": msg }));
            json!({ "healthy": false, "checks": checks, "error": format!("Cannot load module: {msg}") })
        }
    }
}

// ─── events.js ──────────────────────────────────────────────────────────────

/// `readPxpipeEvents({ sinceMs, limit })` — rotated file first, then current.
fn read_pxpipe_events(since_ms: Option<i64>, limit: Option<usize>) -> Vec<Value> {
    let mut events: Vec<Value> = Vec::new();
    for file in [rotated_file(), events_file()] {
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            let Ok(ev) = serde_json::from_str::<Value>(line) else { continue };
            if let Some(since) = since_ms {
                if ev.get("ts").and_then(Value::as_i64).unwrap_or(0) < since {
                    continue;
                }
            }
            events.push(ev);
        }
    }
    events.sort_by_key(|e| e.get("ts").and_then(Value::as_i64).unwrap_or(0));
    if let Some(limit) = limit {
        if events.len() > limit {
            return events.split_off(events.len() - limit);
        }
    }
    events
}

/// Fire-and-forget append with 5 MiB rotation. Used by the request pipeline.
pub fn append_pxpipe_event(event: Value) {
    let dir = pxpipe_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let file = events_file();
    if let Ok(meta) = std::fs::metadata(&file) {
        if meta.len() > MAX_EVENTS_FILE_BYTES {
            let _ = std::fs::rename(&file, rotated_file());
        }
    }
    append_file(&file, &format!("{}\n", stamp_event(event, now_ms())));
}

/// `{ ts: Date.now(), ...event }` — the caller's `ts` wins (JS spread order).
fn stamp_event(event: Value, now: i64) -> Value {
    let mut obj = match event {
        Value::Object(m) => m,
        other => Map::from_iter([("event".to_string(), other)]),
    };
    obj.entry("ts".to_string()).or_insert_with(|| json!(now));
    Value::Object(obj)
}

fn empty_totals() -> Value {
    json!({
        "requests": 0, "compressed": 0, "bypassed": 0, "errors": 0,
        "tokensBeforeEst": 0, "tokensAfterEst": 0, "tokensSavedEst": 0, "savedPct": 0,
        "imagesGenerated": 0, "compressionTimeMs": 0, "avgCompressionMs": 0,
    })
}

fn u(o: &Value, k: &str) -> i64 {
    o.get(k).and_then(Value::as_i64).unwrap_or(0)
}

fn accumulate(totals: &mut Value, ev: &Value) {
    totals["requests"] = json!(u(totals, "requests") + 1);
    let applied = ev.get("applied").and_then(Value::as_bool).unwrap_or(false);
    if applied {
        totals["compressed"] = json!(u(totals, "compressed") + 1);
        totals["tokensBeforeEst"] = json!(u(totals, "tokensBeforeEst") + u(ev, "tokensBeforeEst"));
        totals["tokensAfterEst"] = json!(u(totals, "tokensAfterEst") + u(ev, "tokensAfterEst"));
        totals["tokensSavedEst"] = json!(u(totals, "tokensSavedEst") + u(ev, "tokensSavedEst"));
        totals["imagesGenerated"] = json!(u(totals, "imagesGenerated") + u(ev, "imageCount"));
        totals["compressionTimeMs"] = json!(u(totals, "compressionTimeMs") + u(ev, "durationMs"));
    } else {
        let reason = ev.get("reason").and_then(Value::as_str).unwrap_or("");
        if reason == "transform_error" || reason == "timeout" {
            totals["errors"] = json!(u(totals, "errors") + 1);
        } else {
            totals["bypassed"] = json!(u(totals, "bypassed") + 1);
        }
    }
}

fn finalize(totals: &mut Value) {
    let before = u(totals, "tokensBeforeEst");
    let saved = u(totals, "tokensSavedEst");
    let saved_pct = if before > 0 {
        ((saved as f64 / before as f64) * 10000.0).round() / 100.0
    } else {
        0.0
    };
    totals["savedPct"] = json!(saved_pct);
    let compressed = u(totals, "compressed");
    let avg = if compressed > 0 {
        ((u(totals, "compressionTimeMs") as f64) / (compressed as f64)).round() as i64
    } else {
        0
    };
    totals["avgCompressionMs"] = json!(avg);
}

/// `getPxpipeStats({ timelineDays, recentLimit })`.
fn pxpipe_stats(timeline_days: i64, recent_limit: usize) -> Value {
    let events = read_pxpipe_events(None, None);
    let now = now_ms();
    let start_of_today = local_midnight_ms(now);

    let mut windows: Map<String, Value> = Map::new();
    for key in ["all", "today", "yesterday", "last7d", "last30d"] {
        windows.insert(key.into(), empty_totals());
    }

    let mut timeline: Vec<(String, Value)> = Vec::new();
    for i in (0..timeline_days).rev() {
        let day_ms = start_of_today - i * DAY_MS;
        let date = utc_date(day_ms);
        timeline.push((
            date.clone(),
            json!({ "date": date, "tokensSavedEst": 0, "compressed": 0, "requests": 0 }),
        ));
    }

    for ev in &events {
        let ts = ev.get("ts").and_then(Value::as_i64).unwrap_or(0);
        accumulate(windows.get_mut("all").expect("all window"), ev);
        if ts >= start_of_today {
            accumulate(windows.get_mut("today").expect("today window"), ev);
        } else if ts >= start_of_today - DAY_MS {
            accumulate(windows.get_mut("yesterday").expect("yesterday window"), ev);
        }
        if ts >= now - 7 * DAY_MS {
            accumulate(windows.get_mut("last7d").expect("7d window"), ev);
        }
        if ts >= now - 30 * DAY_MS {
            accumulate(windows.get_mut("last30d").expect("30d window"), ev);
        }

        let key = utc_date(ts);
        if let Some((_, bucket)) = timeline.iter_mut().find(|(d, _)| *d == key) {
            bucket["requests"] = json!(u(bucket, "requests") + 1);
            if ev.get("applied").and_then(Value::as_bool).unwrap_or(false) {
                bucket["compressed"] = json!(u(bucket, "compressed") + 1);
                bucket["tokensSavedEst"] = json!(u(bucket, "tokensSavedEst") + u(ev, "tokensSavedEst"));
            }
        }
    }

    for value in windows.values_mut() {
        finalize(value);
    }

    // `slice(-limit).reverse()`: the last N events, newest first.
    let recent: Vec<Value> = events.iter().rev().take(recent_limit).cloned().collect();

    json!({
        "windows": Value::Object(windows),
        "timeline": timeline.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
        "recent": recent,
    })
}

fn local_midnight_ms(now: i64) -> i64 {
    use chrono::{Local, TimeZone};
    let Some(dt) = Local.timestamp_millis_opt(now).single() else { return now };
    let midnight = dt.date_naive().and_hms_opt(0, 0, 0).unwrap_or_default();
    match Local.from_local_datetime(&midnight).single() {
        Some(d) => d.timestamp_millis(),
        None => now,
    }
}

fn utc_date(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_else(|| chrono::DateTime::from_timestamp(0, 0).expect("epoch"))
        .format("%Y-%m-%d")
        .to_string()
}

// ─── Handlers ───────────────────────────────────────────────────────────────

/// GET|POST /api/pxpipe/health
pub async fn health() -> Response {
    Json(run_health_check().await).into_response()
}

/// POST /api/pxpipe/install
pub async fn install() -> Response {
    match install_pxpipe().await {
        Ok(info) => {
            let health = run_health_check().await;
            let mut out = info;
            out["health"] = health;
            Json(out).into_response()
        }
        Err((status, body)) => (status, Json(body)).into_response(),
    }
}

/// GET /api/pxpipe/logs?limit=
pub async fn logs(Query(params): Query<HashMap<String, String>>) -> Response {
    let limit = parse_limit(&params, 100);
    let mut events = read_pxpipe_events(None, Some(limit));
    events.reverse();
    Json(json!({
        "installLog": install_log_tail(MAX_LOG_LINES),
        "events": events,
    }))
    .into_response()
}

/// POST /api/pxpipe/restart
pub async fn restart() -> Response {
    unload_pxpipe();
    match load_pxpipe().await {
        Ok(_) => Json(pxpipe_status()).into_response(),
        Err((status, body)) => (status, Json(body)).into_response(),
    }
}

/// POST /api/pxpipe/start
pub async fn start(ext: Option<Extension<Db>>) -> Response {
    let settings = settings_of(ext);
    if install_info().get("installed").and_then(Value::as_bool) != Some(true) {
        if settings.get("pxpipeAutoInstall").and_then(Value::as_bool) != Some(true) {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "PXPIPE is not installed", "code": "NOT_INSTALLED" })),
            )
                .into_response();
        }
        if let Err((status, body)) = install_pxpipe().await {
            return (status, Json(body)).into_response();
        }
    }
    match load_pxpipe().await {
        Ok(_) => Json(pxpipe_status()).into_response(),
        Err((status, body)) => (status, Json(body)).into_response(),
    }
}

/// GET /api/pxpipe/stats?limit=
pub async fn stats(Query(params): Query<HashMap<String, String>>) -> Response {
    Json(pxpipe_stats(30, parse_limit(&params, 100))).into_response()
}

/// GET /api/pxpipe/status
pub async fn status(ext: Option<Extension<Db>>) -> Response {
    let settings = settings_of(ext);
    let mut out = pxpipe_status();
    out["enabled"] = json!(settings.get("pxpipeEnabled").and_then(Value::as_bool).unwrap_or(false));
    out["autoInstall"] = json!(settings.get("pxpipeAutoInstall").and_then(Value::as_bool).unwrap_or(false));
    out["minChars"] = settings.get("pxpipeMinChars").cloned().unwrap_or(Value::Null);
    out["timeoutMs"] = settings.get("pxpipeTimeoutMs").cloned().unwrap_or(Value::Null);
    Json(out).into_response()
}

/// POST /api/pxpipe/stop
pub async fn stop() -> Response {
    let was_loaded = unload_pxpipe();
    // `{ stopped, ...getPxpipeStatus() }`
    let mut out = pxpipe_status();
    if let Value::Object(ref mut map) = out {
        let mut merged = Map::new();
        merged.insert("stopped".to_string(), json!(was_loaded));
        merged.append(map);
        *map = merged;
    }
    Json(out).into_response()
}

fn parse_limit(params: &HashMap<String, String>, default: usize) -> usize {
    params
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
        .min(500)
}

/// Best-effort settings read: a DB failure behaves like an empty settings object.
fn settings_of(ext: Option<Extension<Db>>) -> Value {
    get_db(ext)
        .ok()
        .and_then(|db| get_settings(&db).ok())
        .unwrap_or_else(|| json!({}))
}

pub fn routes() -> Router {
    Router::new()
        .route("/api/pxpipe/health", get(health).post(health))
        .route("/api/pxpipe/install", post(install))
        .route("/api/pxpipe/logs", get(logs))
        .route("/api/pxpipe/restart", post(restart))
        .route("/api/pxpipe/start", post(start))
        .route("/api/pxpipe/stats", get(stats))
        .route("/api/pxpipe/status", get(status))
        .route("/api/pxpipe/stop", post(stop))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_defaults_and_caps() {
        let mut params = HashMap::new();
        assert_eq!(parse_limit(&params, 100), 100);
        params.insert("limit".into(), "5".into());
        assert_eq!(parse_limit(&params, 100), 5);
        params.insert("limit".into(), "9999".into());
        assert_eq!(parse_limit(&params, 100), 500);
        params.insert("limit".into(), "0".into());
        assert_eq!(parse_limit(&params, 100), 100);
    }

    #[test]
    fn totals_accumulate_applied_and_error_events() {
        let mut totals = empty_totals();
        accumulate(
            &mut totals,
            &json!({ "applied": true, "tokensBeforeEst": 1000, "tokensSavedEst": 250, "durationMs": 40 }),
        );
        accumulate(&mut totals, &json!({ "applied": false, "reason": "small" }));
        accumulate(&mut totals, &json!({ "applied": false, "reason": "timeout" }));
        finalize(&mut totals);
        assert_eq!(totals["requests"], 3);
        assert_eq!(totals["compressed"], 1);
        assert_eq!(totals["bypassed"], 1);
        assert_eq!(totals["errors"], 1);
        assert_eq!(totals["savedPct"], 25.0);
        assert_eq!(totals["avgCompressionMs"], 40);
    }

    #[test]
    fn stamp_event_keeps_a_caller_supplied_ts() {
        // JS: `JSON.stringify({ ts: Date.now(), ...event })` — spread wins.
        let stamped = stamp_event(json!({ "applied": true, "ts": 1000 }), 5000);
        assert_eq!(stamped["ts"], 1000);

        // No caller ts: the append time is filled in.
        let stamped = stamp_event(json!({ "applied": true }), 5000);
        assert_eq!(stamped["ts"], 5000);

        // Non-object events keep the payload under `event`.
        let stamped = stamp_event(json!("raw"), 5000);
        assert_eq!(stamped["event"], "raw");
        assert_eq!(stamped["ts"], 5000);
    }
}
