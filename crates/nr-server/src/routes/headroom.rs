//! Headroom compression-proxy routes. Faithful port of
//! `src/app/api/headroom/**` plus `src/lib/headroom/{detect,process}.js`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Extension, Query, Request};
use axum::http::header::HeaderName;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use nr_core::http::{client_for, HeaderList};
use nr_db::{get_settings, Db};
use serde_json::{json, Value};

use super::api::common::get_db;

const HEADROOM_COMPRESSION_EXTRAS: &[&str] = &["code", "ml"];
const HEADROOM_HEALTH_TIMEOUT_MS: u64 = 1500;
const DEFAULT_PORT: u16 = 8787;
const STARTUP_TIMEOUT_MS: u64 = 8000;

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

const DASHBOARD_PREFIX: &str = "/api/headroom/proxy";

fn headroom_dir() -> PathBuf {
    nr_core::paths::data_dir().join("headroom")
}

fn pid_file() -> PathBuf {
    headroom_dir().join("proxy.pid")
}

fn log_file() -> PathBuf {
    headroom_dir().join("proxy.log")
}

fn install_log_file() -> PathBuf {
    headroom_dir().join("install.log")
}

/// `DEFAULT_HEADROOM_URL` (`HEADROOM_URL` override).
fn default_headroom_url() -> String {
    std::env::var("HEADROOM_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("http://localhost:{DEFAULT_PORT}"))
}

/// `EXTRA_BINS` — headroom installs its scripts under Python dirs a
/// packaged/launchd environment's PATH misses.
fn extra_bins() -> Vec<String> {
    if cfg!(windows) {
        let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
        let roaming = std::env::var("APPDATA").unwrap_or_default();
        let mut out = Vec::new();
        for v in ["313", "312", "311", "310"] {
            out.push(format!("{local}\\Programs\\Python\\Python{v}\\Scripts"));
        }
        out.push(format!("{roaming}\\Python\\Python313\\Scripts"));
        out
    } else {
        let home = nr_core::paths::home_dir();
        vec![
            "/usr/local/bin".into(),
            "/opt/homebrew/bin".into(),
            "/Library/Frameworks/Python.framework/Versions/3.13/bin".into(),
            "/Library/Frameworks/Python.framework/Versions/3.12/bin".into(),
            "/Library/Frameworks/Python.framework/Versions/3.11/bin".into(),
            "/Library/Frameworks/Python.framework/Versions/3.10/bin".into(),
            home.join(".local/bin").to_string_lossy().into_owned(),
            "/usr/bin".into(),
            "/bin".into(),
        ]
    }
}

fn extended_path() -> String {
    let mut parts = extra_bins();
    if let Ok(p) = std::env::var("PATH") {
        parts.push(p);
    }
    parts.retain(|p| !p.is_empty());
    parts.join(if cfg!(windows) { ";" } else { ":" })
}

fn which_cmd() -> &'static str {
    if cfg!(windows) {
        "where"
    } else {
        "which"
    }
}

/// `findHeadroomBinary()`: first path printed by `which headroom`.
fn find_headroom_binary() -> Option<String> {
    let out = std::process::Command::new(which_cmd())
        .arg("headroom")
        .env("PATH", extended_path())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
}

const PYTHON_CANDIDATES: &[&str] = &["python3.13", "python3.12", "python3.11", "python3.10", "python3", "python"];

fn python_candidates() -> Vec<String> {
    let mut list = Vec::new();
    if let Some(bin) = find_headroom_binary() {
        if let Some(parent) = Path::new(&bin).parent() {
            let names: &[&str] = if cfg!(windows) {
                &["python.exe", "python3.exe"]
            } else {
                &["python3", "python3.13", "python"]
            };
            for name in names {
                list.push(parent.join(name).to_string_lossy().into_owned());
            }
        }
    }
    for dir in extra_bins() {
        if dir.is_empty() {
            continue;
        }
        for name in PYTHON_CANDIDATES {
            let file = if cfg!(windows) { format!("{name}.exe") } else { (*name).to_string() };
            list.push(Path::new(&dir).join(file).to_string_lossy().into_owned());
        }
    }
    list.extend(PYTHON_CANDIDATES.iter().map(|s| (*s).to_string()));
    list
}

fn parse_major_minor(text: &str) -> Option<(u32, u32)> {
    let re = regex::Regex::new(r"(\d+)\.(\d+)").ok()?;
    let caps = re.captures(text)?;
    Some((caps.get(1)?.as_str().parse().ok()?, caps.get(2)?.as_str().parse().ok()?))
}

/// `findPython310()`: first interpreter >= 3.10 that also sees `headroom-ai`.
fn find_python310() -> Option<String> {
    let mut fallback: Option<String> = None;
    for candidate in python_candidates() {
        let Ok(out) = std::process::Command::new(&candidate)
            .arg("--version")
            .env("PATH", extended_path())
            .output()
        else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        // `python --version` writes to stdout on 3.4+, stderr before that.
        let ver = if stdout.trim().is_empty() {
            String::from_utf8_lossy(&out.stderr).into_owned()
        } else {
            stdout.into_owned()
        };
        let Some((major, minor)) = parse_major_minor(&ver) else { continue };
        if !(major > 3 || (major == 3 && minor >= 10)) {
            continue;
        }
        if fallback.is_none() {
            fallback = Some(candidate.clone());
        }
        let shows = std::process::Command::new(&candidate)
            .args(["-m", "pip", "show", "headroom-ai"])
            .env("PATH", extended_path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if shows {
            return Some(candidate);
        }
    }
    fallback
}

fn no_extras() -> Value {
    json!({ "installed": false, "version": Value::Null, "extras": { "code": false, "ml": false } })
}

/// `getInstalledHeadroomExtras(python)`: `{ installed, version, extras }`
/// answered from one `pip list --format=json` call.
fn installed_headroom_extras(python: Option<&str>) -> Value {
    let owned;
    let py = match python {
        Some(p) => p,
        None => match find_python310() {
            Some(p) => {
                owned = p;
                &owned
            }
            None => return no_extras(),
        },
    };
    let Ok(out) = std::process::Command::new(py)
        .args(["-m", "pip", "list", "--format=json", "--disable-pip-version-check"])
        .env("PATH", extended_path())
        .output()
    else {
        return no_extras();
    };
    if !out.status.success() {
        return no_extras();
    }
    let packages: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let names: Vec<String> = packages
        .iter()
        .filter_map(|p| p.get("name").and_then(Value::as_str).map(|s| s.to_lowercase()))
        .collect();
    let has = |n: &str| names.iter().any(|x| x == n);
    if !has("headroom-ai") {
        return no_extras();
    }
    let version = packages
        .iter()
        .find(|p| {
            p.get("name")
                .and_then(Value::as_str)
                .map(|s| s.to_lowercase())
                .as_deref()
                == Some("headroom-ai")
        })
        .and_then(|p| p.get("version").cloned())
        .unwrap_or(Value::Null);
    json!({
        "installed": true,
        "version": version,
        "extras": {
            "code": has("tree-sitter") || has("tree-sitter-language-pack"),
            "ml": has("torch") || has("huggingface-hub"),
        },
    })
}

fn is_loopback_headroom_url(url: &str) -> bool {
    const LOOPBACK: &[&str] = &["localhost", "127.0.0.1", "::1", "[::1]", "0.0.0.0"];
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| LOOPBACK.contains(&h)))
        .unwrap_or(false)
}

/// `probeProxyRunning(url)`: `GET {url}/health` returns 2xx.
async fn probe_proxy_running(url: &str) -> bool {
    if url.is_empty() {
        return false;
    }
    let base = url.trim_end_matches('/');
    let Ok(client) = client_for(None, false) else { return false };
    match tokio::time::timeout(
        Duration::from_millis(HEADROOM_HEALTH_TIMEOUT_MS),
        client.get(format!("{base}/health")).send(),
    )
    .await
    {
        Ok(Ok(res)) => res.status().is_success(),
        _ => false,
    }
}

// ─── process.js ─────────────────────────────────────────────────────────────

fn ensure_dir() {
    let _ = std::fs::create_dir_all(headroom_dir());
}

fn read_pid() -> Option<i64> {
    std::fs::read_to_string(pid_file()).ok()?.trim().parse().ok()
}

fn write_pid(pid: u32) {
    ensure_dir();
    let _ = std::fs::write(pid_file(), pid.to_string());
}

fn clear_pid() {
    let _ = std::fs::remove_file(pid_file());
}

#[cfg(unix)]
fn signal(pid: i64, sig: i32) {
    if pid <= 0 {
        return;
    }
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe {
        kill(pid as i32, sig);
    }
}

/// `isPidAlive(pid)` via `kill(pid, 0)`.
fn is_pid_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    #[cfg(unix)]
    {
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        unsafe { kill(pid as i32, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        // shortcut: no signal-0 probe on Windows; a recorded pid counts as alive.
        true
    }
}

fn get_managed_pid() -> Option<i64> {
    read_pid().filter(|pid| is_pid_alive(*pid))
}

fn extras_proxy_args(code_aware: bool, kompress: bool) -> Vec<String> {
    let mut args = Vec::new();
    if code_aware {
        args.push("--code-aware".into());
    }
    if !kompress {
        args.push("--disable-kompress".into());
    }
    args
}

type HeadroomResult = Result<Value, (StatusCode, Value)>;

fn headroom_err(status: StatusCode, message: impl Into<String>, code: Value) -> (StatusCode, Value) {
    (status, json!({ "error": message.into(), "code": code }))
}

/// `startHeadroomProxy({ port, codeAware, kompress })`.
async fn start_headroom_proxy(port: u16, code_aware: bool, kompress: bool) -> HeadroomResult {
    let safe_port = if port > 0 { port } else { DEFAULT_PORT };
    let Some(binary) = find_headroom_binary() else {
        return Err(headroom_err(
            StatusCode::BAD_REQUEST,
            "Headroom CLI not installed",
            json!("NOT_INSTALLED"),
        ));
    };
    if let Some(existing) = get_managed_pid() {
        return Ok(json!({ "pid": existing, "alreadyRunning": true }));
    }

    ensure_dir();
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file())
        .map_err(|e| headroom_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), Value::Null))?;
    let log_err = log
        .try_clone()
        .map_err(|e| headroom_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), Value::Null))?;

    let mut child = tokio::process::Command::new(&binary)
        .arg("proxy")
        .arg("--port")
        .arg(safe_port.to_string())
        .args(extras_proxy_args(code_aware, kompress))
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(log_err)
        .kill_on_drop(false)
        .spawn()
        .map_err(|e| {
            headroom_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to spawn headroom proxy: {e}"),
                json!("SPAWN_FAILED"),
            )
        })?;

    let Some(pid) = child.id() else {
        return Err(headroom_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to spawn headroom proxy",
            json!("SPAWN_FAILED"),
        ));
    };
    write_pid(pid);
    // The child keeps running detached; the pid file is our handle to it.
    let _ = &mut child;

    // Success = still alive after the startup window; failure = exited early.
    tokio::time::sleep(Duration::from_millis(STARTUP_TIMEOUT_MS)).await;
    if is_pid_alive(pid as i64) {
        Ok(json!({ "pid": pid, "alreadyRunning": false }))
    } else {
        clear_pid();
        Err(headroom_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "headroom proxy exited during startup — see proxy.log",
            json!("EARLY_EXIT"),
        ))
    }
}

/// `stopHeadroomProxy()`: `{ stopped: true, pid }` or `{ stopped: false, reason }`.
fn stop_headroom_proxy() -> Value {
    let Some(pid) = get_managed_pid() else {
        return json!({ "stopped": false, "reason": "not_running" });
    };
    #[cfg(unix)]
    {
        signal(pid, 15);
        // Give it a moment, then force if still alive.
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(2000));
            if is_pid_alive(pid) {
                signal(pid, 9);
            }
        });
    }
    clear_pid();
    json!({ "stopped": true, "pid": pid })
}

/// `restartHeadroomProxy(opts)`: stop the managed proxy, wait, start again.
async fn restart_headroom_proxy(port: u16, code_aware: bool, kompress: bool) -> HeadroomResult {
    if let Some(pid) = get_managed_pid() {
        #[cfg(unix)]
        signal(pid, 15);
        for _ in 0..30 {
            if !is_pid_alive(pid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if is_pid_alive(pid) {
            #[cfg(unix)]
            signal(pid, 9);
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        clear_pid();
    }
    start_headroom_proxy(port, code_aware, kompress).await
}

fn log_tail(path: &Path, max_lines: usize) -> String {
    let Ok(content) = std::fs::read_to_string(path) else { return String::new() };
    let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

fn extra_markers(extra: &str) -> &'static [&'static str] {
    if extra == "code" {
        &["tree-sitter", "tree-sitter-language-pack"]
    } else {
        &["torch", "huggingface-hub"]
    }
}

/// `installHeadroomExtras(extras)`: pip install on top of the `proxy` base.
async fn install_headroom_extras(requested: Vec<String>) -> HeadroomResult {
    let requested: Vec<String> = requested
        .into_iter()
        .filter(|e| HEADROOM_COMPRESSION_EXTRAS.contains(&e.as_str()))
        .collect();
    let Some(py) = find_python310() else {
        return Err(headroom_err(StatusCode::BAD_REQUEST, "Python >= 3.10 not found", json!("NO_PYTHON")));
    };
    if find_headroom_binary().is_none() {
        return Err(headroom_err(
            StatusCode::BAD_REQUEST,
            "headroom-ai not installed (run `pip install headroom-ai[proxy]` first)",
            json!("NOT_INSTALLED"),
        ));
    }
    let mut extras_list = vec!["proxy".to_string()];
    extras_list.extend(requested.iter().cloned());
    let spec = format!("headroom-ai[{}]", extras_list.join(","));

    let code = run_pip(&py, &["-m", "pip", "install", "--upgrade", &spec]).await?;
    if code != 0 {
        return Err(headroom_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("pip install exited with code={code} — see headroom/install.log"),
            json!("INSTALL_FAILED"),
        ));
    }
    let mut out = installed_headroom_extras(Some(&py));
    out["success"] = json!(true);
    out["code"] = json!(code);
    out["spec"] = json!(spec);
    out["extras"] = json!(requested);
    Ok(out)
}

/// `uninstallHeadroomExtras(extras)`: pip uninstall the marker packages.
async fn uninstall_headroom_extras(requested: Vec<String>) -> HeadroomResult {
    let requested: Vec<String> = requested
        .into_iter()
        .filter(|e| HEADROOM_COMPRESSION_EXTRAS.contains(&e.as_str()))
        .collect();
    let Some(py) = find_python310() else {
        return Err(headroom_err(StatusCode::BAD_REQUEST, "Python >= 3.10 not found", json!("NO_PYTHON")));
    };
    let mut pkgs: Vec<String> = Vec::new();
    for extra in &requested {
        for marker in extra_markers(extra) {
            if !pkgs.iter().any(|p| p == marker) {
                pkgs.push((*marker).to_string());
            }
        }
    }
    if pkgs.is_empty() {
        return Err(headroom_err(
            StatusCode::BAD_REQUEST,
            "No valid extras to remove",
            json!("INVALID_EXTRAS"),
        ));
    }

    let mut args: Vec<String> = vec!["-m".into(), "pip".into(), "uninstall".into(), "-y".into()];
    args.extend(pkgs.iter().cloned());
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let code = run_pip(&py, &arg_refs).await?;
    if code != 0 {
        return Err(headroom_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("pip uninstall exited with code={code} — see headroom/install.log"),
            json!("UNINSTALL_FAILED"),
        ));
    }
    let mut out = installed_headroom_extras(Some(&py));
    out["success"] = json!(true);
    out["code"] = json!(code);
    out["removed"] = json!(pkgs);
    out["extras"] = json!(requested);
    Ok(out)
}

/// Truncate the install log ("w"), spawn pip, return the exit code.
async fn run_pip(python: &str, args: &[&str]) -> Result<i32, (StatusCode, Value)> {
    ensure_dir();
    let log = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(install_log_file())
        .map_err(|e| headroom_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), Value::Null))?;
    let log_err = log
        .try_clone()
        .map_err(|e| headroom_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), Value::Null))?;

    let status = tokio::process::Command::new(python)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(log_err)
        .status()
        .await
        .map_err(|e| headroom_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), Value::Null))?;
    Ok(status.code().unwrap_or(-1))
}

// ─── settings helpers ───────────────────────────────────────────────────────

/// Best-effort settings read: a DB failure behaves like an empty settings object.
fn settings_of(ext: &Option<Extension<Db>>) -> Value {
    let db = match ext {
        Some(Extension(db)) => Some(db.clone()),
        None => get_db(None).ok(),
    };
    db.and_then(|db| get_settings(&db).ok()).unwrap_or_else(|| json!({}))
}

/// `settings.headroomUrl || DEFAULT_HEADROOM_URL`.
fn headroom_url(settings: &Value) -> String {
    settings
        .get("headroomUrl")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(default_headroom_url)
}

/// `parsePortFromUrl(url) || 8787`.
fn parse_port_from_url(url: &str) -> u16 {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.port())
        .filter(|p| *p > 0)
        .unwrap_or(DEFAULT_PORT)
}

/// `{ port, codeAware, kompress }` from settings.
fn proxy_opts(settings: &Value) -> (u16, bool, bool) {
    let port = parse_port_from_url(&headroom_url(settings));
    let code_aware = settings.get("headroomCodeAware").and_then(Value::as_bool) == Some(true);
    let kompress = settings.get("headroomKompress").and_then(Value::as_bool) != Some(false);
    (port, code_aware, kompress)
}

fn external_proxy_error() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": "External Headroom proxies must be started outside 9Router",
            "code": "EXTERNAL_PROXY",
        })),
    )
        .into_response()
}

// ─── Handlers ───────────────────────────────────────────────────────────────

/// GET /api/headroom/status
pub async fn get_status(ext: Option<Extension<Db>>) -> Response {
    let settings = settings_of(&ext);
    let url = headroom_url(&settings);
    let path = find_headroom_binary();
    let python = find_python310();
    let installed = path.is_some();
    let running = probe_proxy_running(&url).await;
    let local_url = is_loopback_headroom_url(&url);
    let extras_status = if installed {
        installed_headroom_extras(python.as_deref())
    } else {
        no_extras()
    };
    let managed_pid = get_managed_pid();

    Json(json!({
        "installed": installed,
        "path": path,
        "running": running,
        "python": python,
        "localUrl": local_url,
        "canStart": installed && local_url,
        "version": extras_status.get("version").cloned().unwrap_or(Value::Null),
        "extras": extras_status
            .get("extras")
            .cloned()
            .unwrap_or_else(|| json!({ "code": false, "ml": false })),
        "url": url,
        "managedPid": managed_pid,
    }))
    .into_response()
}

/// `{ success: true, ...result }`.
fn success_with(result: Value) -> Value {
    let mut out = json!({ "success": true });
    if let (Some(obj), Value::Object(extra)) = (out.as_object_mut(), result) {
        obj.extend(extra);
    }
    out
}

/// POST /api/headroom/start
pub async fn post_start(ext: Option<Extension<Db>>) -> Response {
    let settings = settings_of(&ext);
    if !is_loopback_headroom_url(&headroom_url(&settings)) {
        return external_proxy_error();
    }
    let (port, code_aware, kompress) = proxy_opts(&settings);
    match start_headroom_proxy(port, code_aware, kompress).await {
        Ok(result) => Json(success_with(result)).into_response(),
        Err((status, body)) => (status, Json(body)).into_response(),
    }
}

/// POST /api/headroom/restart
pub async fn post_restart(ext: Option<Extension<Db>>) -> Response {
    let settings = settings_of(&ext);
    if !is_loopback_headroom_url(&headroom_url(&settings)) {
        return external_proxy_error();
    }
    let (port, code_aware, kompress) = proxy_opts(&settings);
    match restart_headroom_proxy(port, code_aware, kompress).await {
        Ok(result) => Json(success_with(result)).into_response(),
        Err((status, body)) => (status, Json(body)).into_response(),
    }
}

/// POST /api/headroom/stop — 200 when stopped, 409 when nothing was running.
pub async fn post_stop() -> Response {
    let result = stop_headroom_proxy();
    let status = if result.get("stopped").and_then(Value::as_bool) == Some(true) {
        StatusCode::OK
    } else {
        StatusCode::CONFLICT
    };
    (status, Json(result)).into_response()
}

/// GET /api/headroom/extras — `?log=1` returns the live install log tail.
pub async fn get_extras(Query(params): Query<HashMap<String, String>>) -> Response {
    if params.get("log").map(String::as_str) == Some("1") {
        let body = json!({ "log": log_tail(&install_log_file(), 15) });
        return Json(body).into_response();
    }
    let python = find_python310();
    let status = installed_headroom_extras(python.as_deref());
    let mut out = json!({ "available": HEADROOM_COMPRESSION_EXTRAS });
    if let (Some(obj), Value::Object(extra)) = (out.as_object_mut(), status) {
        obj.extend(extra);
    }
    Json(out).into_response()
}

/// POST /api/headroom/extras — install the requested compression extras.
pub async fn post_extras(body: Option<Json<Value>>) -> Response {
    match install_headroom_extras(requested_extras(body)).await {
        Ok(result) => Json(result).into_response(),
        Err((status, body)) => (status, Json(body)).into_response(),
    }
}

/// DELETE /api/headroom/extras — remove the marker packages of the extras.
pub async fn delete_extras(body: Option<Json<Value>>) -> Response {
    match uninstall_headroom_extras(requested_extras(body)).await {
        Ok(result) => Json(result).into_response(),
        Err((status, body)) => (status, Json(body)).into_response(),
    }
}

fn requested_extras(body: Option<Json<Value>>) -> Vec<String> {
    body.and_then(|Json(v)| v.get("extras").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|e| e.as_str().map(String::from))
        .collect()
}

// ─── /api/headroom/proxy/{*path} ────────────────────────────────────────────

/// `rewriteDashboardHtml`: prefix the bundled dashboard's root-absolute fetches.
fn rewrite_dashboard_html(html: &str) -> String {
    // No look-ahead in the `regex` crate: match the path and re-emit it.
    let re = regex::Regex::new(r"fetch\('(/(?:stats|health|stats-history|transformations/feed))")
        .expect("static regex");
    re.replace_all(html, format!("fetch('{DASHBOARD_PREFIX}$1")).into_owned()
}

/// `forwardedHeaders`: drop hop-by-hop + `host`; drop credentials for
/// non-loopback targets so viewer auth never leaks to a remote Headroom.
fn forwarded_headers(headers: &HeaderMap, target: &url::Url) -> HeaderList {
    let mut out = HeaderList::new();
    for (name, value) in headers.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if HOP_BY_HOP.contains(&lower.as_str()) || lower == "host" {
            continue;
        }
        if let Ok(v) = value.to_str() {
            out.insert(name.as_str().to_string(), v.to_string());
        }
    }
    let host = target
        .host_str()
        .unwrap_or("")
        .trim_matches(|c| c == '[' || c == ']')
        .to_ascii_lowercase();
    if !matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1") {
        out.shift_remove("cookie");
        out.shift_remove("authorization");
    }
    out
}

/// Every HTTP method (JS exports `proxy` as GET/POST/PUT/PATCH/DELETE/HEAD/OPTIONS).
pub async fn proxy(ext: Option<Extension<Db>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let sub = parts
        .uri
        .path()
        .strip_prefix("/api/headroom/proxy")
        .unwrap_or("")
        .to_string();
    let bytes = axum::body::to_bytes(body, 16 * 1024 * 1024).await.unwrap_or_default();
    proxy_to_headroom(
        &ext,
        parts.method,
        &sub,
        parts.uri.query(),
        parts.headers,
        bytes,
        None,
    )
    .await
}

/// `base_url` overrides the settings URL; production callers pass `None`.
async fn proxy_to_headroom(
    ext: &Option<Extension<Db>>,
    method: Method,
    uri_path: &str,
    query: Option<&str>,
    headers: HeaderMap,
    body: axum::body::Bytes,
    base_url: Option<&str>,
) -> Response {
    let settings = settings_of(ext);
    let base = match base_url {
        Some(url) => url.to_string(),
        None => headroom_url(&settings),
    };
    let mut target = match url::Url::parse(&base) {
        Ok(u) if u.scheme() == "http" || u.scheme() == "https" => u,
        Ok(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Headroom URL must use http or https" })),
            )
                .into_response()
        }
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })))
                .into_response()
        }
    };

    let path = uri_path.trim_start_matches('/');
    target.set_path(&format!("/{path}"));
    target.set_query(query);
    target.set_fragment(None);

    let has_body = !matches!(method, Method::GET | Method::HEAD);
    let mut req = nr_core::http::FetchRequest::new(method, target.to_string());
    req.headers = forwarded_headers(&headers, &target);
    if has_body && !body.is_empty() {
        req.body = Some(body);
    }

    let response = match nr_core::http::proxy_fetch(req, None).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })))
                .into_response()
        }
    };

    let status = StatusCode::from_u16(response.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut out_headers = HeaderMap::new();
    for (name, value) in response.headers.iter() {
        if HOP_BY_HOP.contains(&name.as_str()) {
            continue;
        }
        let value = value.to_str().unwrap_or("");
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_str(value),
        ) {
            out_headers.insert(n, v);
        }
    }

    // The bundled dashboard calls root-absolute endpoints; rewrite them onto the
    // proxy prefix so it keeps working behind this route.
    if path == "dashboard" {
        let content_type = out_headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if content_type.contains("text/html") {
            out_headers.remove("content-length");
            let text = response.text().await.unwrap_or_default();
            let mut resp = Response::new(Body::from(rewrite_dashboard_html(&text)));
            *resp.status_mut() = status;
            *resp.headers_mut() = out_headers;
            return resp;
        }
    }

    let mut resp = Response::new(Body::from_stream(response.body));
    *resp.status_mut() = status;
    *resp.headers_mut() = out_headers;
    resp
}

pub fn routes() -> Router {
    Router::new()
        .route("/api/headroom/status", get(get_status))
        .route("/api/headroom/start", post(post_start))
        .route("/api/headroom/restart", post(post_restart))
        .route("/api/headroom/stop", post(post_stop))
        .route(
            "/api/headroom/extras",
            get(get_extras).post(post_extras).delete(delete_extras),
        )
        .route("/api/headroom/proxy/{*path}", any(proxy))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_html_rewrite_prefixes_known_endpoints() {
        let html = "<script>fetch('/stats')</script><script>fetch('/health')</script>";
        let out = rewrite_dashboard_html(html);
        assert!(out.contains("fetch('/api/headroom/proxy/stats')"), "{out}");
        assert!(out.contains("fetch('/api/headroom/proxy/health')"), "{out}");
    }

    #[test]
    fn parse_port_falls_back_to_default() {
        assert_eq!(parse_port_from_url("http://localhost:9999"), 9999);
        assert_eq!(parse_port_from_url("http://localhost"), DEFAULT_PORT);
        assert_eq!(parse_port_from_url("not a url"), DEFAULT_PORT);
    }

    #[test]
    fn loopback_detection_matches_js() {
        assert!(is_loopback_headroom_url("http://localhost:8787"));
        assert!(is_loopback_headroom_url("http://127.0.0.1:8787"));
        assert!(!is_loopback_headroom_url("https://headroom.example.com"));
        assert!(!is_loopback_headroom_url("nonsense"));
    }

    #[test]
    fn non_loopback_targets_lose_viewer_credentials() {
        let mut headers = HeaderMap::new();
        headers.insert("cookie", "session=1".parse().unwrap());
        headers.insert("authorization", "Bearer x".parse().unwrap());
        headers.insert("x-keep", "1".parse().unwrap());

        let remote = url::Url::parse("https://headroom.example.com").unwrap();
        let kept = forwarded_headers(&headers, &remote);
        assert!(!kept.contains_key("cookie"));
        assert!(!kept.contains_key("authorization"));
        assert_eq!(kept.get("x-keep").map(String::as_str), Some("1"));

        let local = url::Url::parse("http://127.0.0.1:8787").unwrap();
        let kept = forwarded_headers(&headers, &local);
        assert!(kept.contains_key("cookie"));
        assert!(kept.contains_key("authorization"));
    }

    /// Stand up a throwaway upstream that answers like the headroom proxy.
    async fn stub_upstream() -> String {
        use axum::routing::get;
        let app = Router::new()
            .route("/stats", get(|| async { Json(json!({ "requests": 3, "savedPct": 12.5 })) }))
            .route(
                "/dashboard",
                get(|| async {
                    (
                        [(axum::http::header::CONTENT_TYPE, "text/html")],
                        "<html><script>fetch('/stats')</script></html>",
                    )
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind stub");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn proxy_passes_through_and_rewrites_dashboard() {
        let base = stub_upstream().await;
        let body = proxy_to_headroom(
            &None,
            Method::GET,
            "/stats",
            None,
            HeaderMap::new(),
            axum::body::Bytes::new(),
            Some(&base),
        )
        .await;
        assert_eq!(body.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(body.into_body(), 64 * 1024).await.expect("body");
        let value: Value = serde_json::from_slice(&bytes).expect("json passthrough");
        assert_eq!(value["requests"], 3);
        assert_eq!(value["savedPct"], 12.5);

        // `dashboard` keeps the HTML content type but gets its fetches prefixed.
        let dash = proxy_to_headroom(
            &None,
            Method::GET,
            "/dashboard",
            None,
            HeaderMap::new(),
            axum::body::Bytes::new(),
            Some(&base),
        )
        .await;
        assert_eq!(dash.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(dash.into_body(), 64 * 1024).await.expect("body");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("/api/headroom/proxy/stats"), "{text}");
    }

    /// Unreachable sidecar: the route reports it, it never fakes success.
    #[tokio::test]
    async fn proxy_reports_an_unreachable_sidecar() {
        let body = proxy_to_headroom(
            &None,
            Method::GET,
            "/stats",
            None,
            HeaderMap::new(),
            axum::body::Bytes::new(),
            Some("http://127.0.0.1:1"),
        )
        .await;
        assert_eq!(body.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = axum::body::to_bytes(body.into_body(), 64 * 1024).await.expect("body");
        let value: Value = serde_json::from_slice(&bytes).expect("json error");
        assert!(value.get("error").is_some(), "{value}");
    }
}
