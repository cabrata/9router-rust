//! MCP stdio→SSE bridge routes. Faithful port of
//! `src/app/api/mcp/[plugin]/{sse,message}/route.js` and
//! `src/lib/mcp/stdioSseBridge.js`.
//!
//! One child process per preset stdio plugin, spawned on demand. Frames are
//! broadcast to every open SSE session; client messages arrive over HTTP POST
//! and are written to the child's stdin.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use axum::body::Body;
use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::broadcast;

const MAX_TEXT_CHARS: usize = 50_000;
const COLLAPSE_THRESHOLD: usize = 30;
const COLLAPSE_KEEP_HEAD: usize = 10;
const COLLAPSE_KEEP_TAIL: usize = 5;

// ─── Preset plugins ─────────────────────────────────────────────────────────

/// `findPlugin(name)` over `LOCAL_STDIO_PLUGINS`. Only preset stdio plugins may
/// spawn; there are no user-defined commands (RCE prevention).
fn find_plugin(name: &str) -> Option<Value> {
    nr_data::export("src/shared/constants/coworkPlugins", "LOCAL_STDIO_PLUGINS")?
        .as_array()?
        .iter()
        .find(|p| p.get("name").and_then(Value::as_str) == Some(name))
        .cloned()
}

// ─── Smart text filter (browsermcp snapshots) ───────────────────────────────

fn strip_noise_blank_nodes(text: &str) -> String {
    // `^\s*-\s*generic:?\s*$` and `^\s*-\s*text:\s*""\s*$` removed line-wise.
    let generic = regex::Regex::new(r"^\s*-\s*generic:?\s*$").expect("static regex");
    let empty_text = regex::Regex::new("^\\s*-\\s*text:\\s*\"\"\\s*$").expect("static regex");
    text.lines()
        .filter(|line| !generic.is_match(line) && !empty_text.is_match(line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `collapseRepeated(text)`: group consecutive lines sharing the same leading
/// indent + role prefix; collapse when the group is >= 30 lines.
fn collapse_repeated(text: &str) -> String {
    let sibling = regex::Regex::new(r"^(\s*)-\s*([a-zA-Z]+)\b").expect("static regex");
    let lines: Vec<&str> = text.split('\n').collect();

    let sibling_at = |k: usize| -> Option<(String, String)> {
        let caps = sibling.captures(lines[k])?;
        Some((caps.get(1)?.as_str().to_string(), caps.get(2)?.as_str().to_string()))
    };

    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some((indent, role)) = sibling_at(i) else {
            out.push(lines[i].to_string());
            i += 1;
            continue;
        };
        let mut j = i;
        while j < lines.len() {
            match sibling_at(j) {
                Some((ind, r)) if ind == indent && r == role => {
                    j += 1;
                    continue;
                }
                _ => {
                    let ln = lines[j];
                    if ln.starts_with(&format!("{indent} ")) || ln.starts_with(&format!("{indent}\t")) {
                        j += 1;
                        continue;
                    }
                    break;
                }
            }
        }
        let group_len = j - i;
        if group_len >= COLLAPSE_THRESHOLD {
            let head_end = find_nth_sibling_end(&lines, i, &indent, &role, COLLAPSE_KEEP_HEAD);
            let tail_start = find_last_n_sibling_start(&lines, j, &indent, &role, COLLAPSE_KEEP_TAIL);
            for line in &lines[i..head_end] {
                out.push((*line).to_string());
            }
            let omitted = group_len - COLLAPSE_KEEP_HEAD - COLLAPSE_KEEP_TAIL;
            out.push(format!("{indent}... [{omitted} similar \"{role}\" items omitted by 9router bridge]"));
            for line in &lines[tail_start..j] {
                out.push((*line).to_string());
            }
        } else {
            for line in &lines[i..j] {
                out.push((*line).to_string());
            }
        }
        i = j;
    }
    out.join("\n")
}

fn find_nth_sibling_end(lines: &[&str], start: usize, indent: &str, role: &str, n: usize) -> usize {
    let re = regex::Regex::new(r"^(\s*)-\s*([a-zA-Z]+)\b").expect("static regex");
    let mut count = 0;
    for (k, line) in lines.iter().enumerate().skip(start) {
        if let Some(caps) = re.captures(line) {
            if caps.get(1).map(|m| m.as_str()) == Some(indent) && caps.get(2).map(|m| m.as_str()) == Some(role) {
                count += 1;
                if count > n {
                    return k;
                }
            }
        }
    }
    lines.len()
}

fn find_last_n_sibling_start(lines: &[&str], end: usize, indent: &str, role: &str, n: usize) -> usize {
    let re = regex::Regex::new(r"^(\s*)-\s*([a-zA-Z]+)\b").expect("static regex");
    let mut positions = Vec::new();
    for (k, line) in lines.iter().enumerate().take(end) {
        if let Some(caps) = re.captures(line) {
            if caps.get(1).map(|m| m.as_str()) == Some(indent) && caps.get(2).map(|m| m.as_str()) == Some(role) {
                positions.push(k);
            }
        }
    }
    if positions.len() > n {
        positions[positions.len() - n]
    } else {
        end
    }
}

/// `smartFilterText(text)`: drop noise nodes, collapse repeats, hard-truncate.
/// Preserves `[ref=eXX]` markers.
fn smart_filter_text(text: &str) -> String {
    if text.len() < 2000 {
        return text.to_string();
    }
    let mut out = strip_noise_blank_nodes(text);
    out = collapse_repeated(&out);
    if out.len() > MAX_TEXT_CHARS {
        let keep = MAX_TEXT_CHARS.saturating_sub(300);
        let mut cut = keep;
        while cut > 0 && !out.is_char_boundary(cut) {
            cut -= 1;
        }
        let head = &out[..cut];
        return format!(
            "{head}\n\n... [truncated {} chars by 9router bridge. Page is large; ask user to scroll/navigate to a specific section, or click an element with the refs shown above]",
            text.len() - head.len()
        );
    }
    out
}

/// `filterFrame(line)`: rewrite text blocks of a JSON-RPC result frame only.
fn filter_frame(line: &str) -> String {
    let Ok(mut msg) = serde_json::from_str::<Value>(line) else { return line.to_string() };
    let Some(content) = msg
        .get_mut("result")
        .and_then(|r| r.get_mut("content"))
        .and_then(Value::as_array_mut)
    else {
        return line.to_string();
    };
    let mut mutated = false;
    for item in content.iter_mut() {
        if item.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        let Some(text) = item.get("text").and_then(Value::as_str).map(String::from) else { continue };
        let filtered = smart_filter_text(&text);
        if filtered != text {
            item["text"] = json!(filtered);
            mutated = true;
        }
    }
    if mutated {
        msg.to_string()
    } else {
        line.to_string()
    }
}

// ─── Bridge registry ────────────────────────────────────────────────────────

struct Bridge {
    stdin: tokio::sync::Mutex<ChildStdin>,
    /// Frames from the child, fanned out to every open session.
    tx: broadcast::Sender<String>,
    /// Open SSE sessions; the last one to leave kills the child.
    sessions: std::sync::atomic::AtomicUsize,
}

impl Bridge {
    /// One connected client. Dropping it releases the child when it was last.
    fn session(self: &Arc<Self>, name: &str) -> SessionGuard {
        self.sessions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        SessionGuard { name: name.to_string(), bridge: self.clone() }
    }
}

struct SessionGuard {
    name: String,
    bridge: Arc<Bridge>,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let remaining = self.bridge.sessions.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) - 1;
        if remaining == 0 {
            let name = self.name.clone();
            tokio::spawn(async move {
                // `unregisterSession`: no sessions left → kill the child.
                BRIDGES.lock().await.remove(&name);
            });
        }
    }
}

static BRIDGES: LazyLock<tokio::sync::Mutex<HashMap<String, Arc<Bridge>>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));

/// `getOrSpawn(name)`: reuse a live child, else spawn one.
async fn get_or_spawn(name: &str) -> Option<Arc<Bridge>> {
    let mut store = BRIDGES.lock().await;
    if let Some(bridge) = store.get(name) {
        return Some(bridge.clone());
    }
    let plugin = find_plugin(name)?;
    let command = plugin.get("command").and_then(Value::as_str)?.to_string();
    let args: Vec<String> = plugin
        .get("args")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let mut child: Child = Command::new(&command)
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(false)
        .spawn()
        .ok()?;

    let stdin = child.stdin.take()?;
    let stdout = child.stdout.take()?;
    let stderr = child.stderr.take();
    let (tx, _rx) = broadcast::channel::<String>(256);
    let bridge = Arc::new(Bridge {
        stdin: tokio::sync::Mutex::new(stdin),
        tx: tx.clone(),
        sessions: std::sync::atomic::AtomicUsize::new(0),
    });

    // Newline-delimited JSON-RPC from the child → every session.
    let name_owned = name.to_string();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(raw)) = reader.next_line().await {
            let raw = raw.trim().to_string();
            if raw.is_empty() {
                continue;
            }
            let frame = filter_frame(&raw);
            // No receivers: the frame is dropped, as in the JS bridge.
            let _ = tx.send(frame);
        }
        BRIDGES.lock().await.remove(&name_owned);
    });

    if let Some(stderr) = stderr {
        let name_owned = name.to_string();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let line = line.trim();
                if !line.is_empty() {
                    tracing::info!("[mcp:{name_owned}] {line}");
                }
            }
        });
    }

    // Reap the child so it cannot linger as a zombie.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });

    store.insert(name.to_string(), bridge.clone());
    Some(bridge)
}

/// `sendToChild(name, jsonRpc)`.
async fn send_to_child(name: &str, message: &Value) -> Result<(), String> {
    let bridge = BRIDGES
        .lock()
        .await
        .get(name)
        .cloned()
        .ok_or_else(|| format!("Bridge not running: {name}"))?;
    let mut stdin = bridge.stdin.lock().await;
    stdin
        .write_all(format!("{message}\n").as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stdin.flush().await.map_err(|e| e.to_string())
}

// ─── Handlers ───────────────────────────────────────────────────────────────

/// GET /api/mcp/{plugin}/sse
pub async fn sse(Path(plugin): Path<String>) -> Response {
    if find_plugin(&plugin).is_none() {
        return (StatusCode::NOT_FOUND, format!("Unknown plugin: {plugin}")).into_response();
    }

    let Some(bridge) = get_or_spawn(&plugin).await else {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("Unknown local plugin: {plugin}")).into_response();
    };
    let mut rx = bridge.tx.subscribe();
    let _guard = bridge.session(&plugin);
    let sid = uuid::Uuid::new_v4().to_string();
    let endpoint = format!("/api/mcp/{plugin}/message?sessionId={sid}");

    let stream = async_stream::stream! {
        yield Ok::<Event, std::convert::Infallible>(
            Event::default().event("endpoint").data(endpoint),
        );
        // The guard lives in the stream so a client disconnect drops it.
        let _guard = _guard;
        loop {
            match rx.recv().await {
                Ok(frame) => yield Ok(Event::default().event("message").data(frame)),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    let mut resp = Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(25)).text("ping"))
        .into_response();
    let headers = resp.headers_mut();
    headers.insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-cache, no-transform"));
    headers.insert(header::CONNECTION, header::HeaderValue::from_static("keep-alive"));
    headers.insert("x-accel-buffering", header::HeaderValue::from_static("no"));
    resp
}

/// POST /api/mcp/{plugin}/message — 202 with an empty body on success.
pub async fn message(Path(plugin): Path<String>, body: Option<Json<Value>>) -> Response {
    if find_plugin(&plugin).is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("Unknown plugin: {plugin}") })),
        )
            .into_response();
    }
    let Some(Json(payload)) = body else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "invalid JSON body" })),
        )
            .into_response();
    };
    // The bridge must be warm: JS returns "Bridge not running" until an SSE
    // session spawned the child.
    if BRIDGES.lock().await.get(&plugin).is_none() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Bridge not running: {plugin}") })),
        )
            .into_response();
    }
    match send_to_child(&plugin, &payload).await {
        Ok(()) => Response::builder()
            .status(StatusCode::ACCEPTED)
            .body(Body::empty())
            .unwrap_or_else(|_| StatusCode::ACCEPTED.into_response()),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response(),
    }
}

pub fn routes() -> Router {
    Router::new()
        .route("/api/mcp/{plugin}/sse", get(sse))
        .route("/api/mcp/{plugin}/message", post(message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_plugin_is_rejected() {
        assert!(find_plugin("definitely-not-a-plugin").is_none());
        // Preset plugin from the dumped coworkPlugins constant.
        assert!(find_plugin("browsermcp").is_some());
    }

    #[test]
    fn filters_noise_and_preserves_small_text() {
        assert_eq!(smart_filter_text("short"), "short");

        let noisy = "  - generic:\n  - text: \"\"\n  - button \"Go\" [ref=e12]\n";
        let out = smart_filter_text(noisy);
        assert!(!out.contains("generic"));
        assert!(out.contains("[ref=e12]"), "{out}");
    }

    #[test]
    fn collapses_long_repeated_sibling_groups() {
        let mut text = String::new();
        for i in 0..40 {
            text.push_str(&format!("  - link \"item{i}\"\n"));
        }
        let out = collapse_repeated(&text);
        assert!(out.contains("similar \"link\" items omitted by 9router bridge"), "{out}");
        assert!(out.contains("item0"));
        assert!(out.contains("item39"));
    }

    #[test]
    fn filter_frame_rewrites_only_text_blocks() {
        let none = "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}";
        assert_eq!(filter_frame(none), none);

        let long = "x".repeat(3000);
        let frame = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": { "content": [{ "type": "text", "text": long }] },
        })
        .to_string();
        // Below the 50k hard cap nothing changes, so the raw line is returned.
        assert_eq!(filter_frame(&frame), frame);
    }
}
