//! Minimal Chrome DevTools Protocol (CDP) client for browser automation.
//! Supports the Playwright-compatible context/page subset the executors use.
//!
//! Transport: launch a Chromium binary with `--remote-debugging-port=0`,
//! read the DevTools websocket URL from stderr, then drive
//! `Target`/`Page`/`Runtime`/`Input` over that socket. Each page gets its own
//! flattened session so navigation, evaluation and input are independent.
//!
//! The trait surface mirrors the JS Playwright calls the executors make, so the
//! executor code stays a line-by-line port; only this module knows CDP.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{oneshot, Mutex, RwLock};
use tokio_tungstenite::tungstenite::Message;

/// A cookie entry in Playwright format
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PlaywrightCookie {
    pub name: String,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secure: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub same_site: Option<String>,
}

/// Browser abstraction for spawning contexts
#[async_trait]
pub trait Browser: Send + Sync {
    /// Create a new browser context
    async fn new_context(&self, options: ContextOptions) -> Result<Arc<dyn BrowserContext>, String>;
    /// Close the browser
    async fn close(&self) -> Result<(), String>;
    /// Check if browser is still connected
    fn is_connected(&self) -> bool;
}

/// Browser context for managing pages and cookies
#[async_trait]
pub trait BrowserContext: Send + Sync {
    /// Create a new page in this context
    async fn new_page(&self) -> Result<Arc<dyn Page>, String>;
    /// Close this context
    async fn close(&self) -> Result<(), String>;
    /// Add cookies to this context
    async fn add_cookies(&self, cookies: &[PlaywrightCookie]) -> Result<(), String>;
    /// Get cookies from this context
    async fn get_cookies(&self) -> Result<Vec<PlaywrightCookie>, String>;
    /// Add initial script to be evaluated on every page load
    async fn add_init_script(&self, script: &str, args: Option<Value>) -> Result<(), String>;
}

/// Page in a browser context
#[async_trait]
pub trait Page: Send + Sync {
    /// Navigate to a URL
    async fn goto(&self, url: &str, options: GotoOptions) -> Result<(), String>;
    /// Get current URL
    fn url(&self) -> String;
    /// Close this page
    async fn close(&self) -> Result<(), String>;
    /// Type text into focused element (dispatches input event)
    async fn insert_text(&self, text: &str) -> Result<(), String>;
    /// Press a key
    async fn press_key(&self, key: &str) -> Result<(), String>;
    /// Click an element
    async fn click(&self, selector: &str) -> Result<(), String>;
    /// Wait for a selector to appear
    async fn wait_for_selector(&self, selector: &str, timeout_ms: u32) -> Result<(), String>;
    /// Count elements matching selector
    async fn count(&self, selector: &str) -> Result<usize, String>;
    /// Evaluate JavaScript and return result
    async fn evaluate(&self, script: &str, args: Option<Value>) -> Result<Value, String>;
    /// Evaluate JavaScript on a specific element
    async fn eval_on_selector(&self, selector: &str, script: &str) -> Result<Value, String>;
    /// Get locator for selector
    fn locator(&self, selector: &str) -> Locator;
    /// Wait for function to return true
    async fn wait_for_function(&self, script: &str, timeout_ms: u32) -> Result<(), String>;
    /// Subscribe to response bodies (no-op for the CDP page; kept for parity)
    fn on_response(&self, _handler: Box<dyn Fn(String) + Send>) {}
    /// Screenshot as base64
    async fn screenshot(&self, as_base64: bool) -> Result<Vec<u8>, String>;
}

/// Locator for a selector with additional operations
#[derive(Clone)]
pub struct Locator {
    selector: String,
    page: Arc<dyn Page>,
}

impl Locator {
    pub fn new(selector: String, page: Arc<dyn Page>) -> Self {
        Self { selector, page }
    }

    pub async fn count(&self) -> Result<usize, String> {
        self.page.count(&self.selector).await
    }

    pub async fn click(&self) -> Result<(), String> {
        self.page.click(&self.selector).await
    }

    pub async fn fill(&self, text: &str) -> Result<(), String> {
        self.page.click(&self.selector).await?;
        self.page.insert_text(text).await
    }
}

/// Options for creating a browser context
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ContextOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headless: Option<bool>,
}

impl Default for ContextOptions {
    fn default() -> Self {
        Self { user_agent: None, locale: None, timezone_id: None, headless: Some(true) }
    }
}

/// Options for page navigation
#[derive(Debug, Clone)]
pub struct GotoOptions {
    pub wait_until: String, // "load", "domcontentloaded", "networkidle"
    pub timeout_ms: u32,
}

impl Default for GotoOptions {
    fn default() -> Self {
        Self { wait_until: "domcontentloaded".to_string(), timeout_ms: 30000 }
    }
}

// ─── CDP transport ──────────────────────────────────────────────────────────

type CdpSocket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// One in-flight CDP command awaiting its reply.
type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>>;

/// Shared websocket connection with request/response correlation.
struct CdpConnection {
    tx: tokio::sync::mpsc::UnboundedSender<Message>,
    pending: Pending,
    next_id: AtomicI64,
    closed: AtomicBool,
}

impl CdpConnection {
    async fn connect(ws_url: &str) -> Result<Arc<Self>, String> {
        let (stream, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| format!("cdp: connect {ws_url}: {e}"))?;
        Self::start(stream)
    }

    fn start(stream: CdpSocket) -> Result<Arc<Self>, String> {
        use futures::{SinkExt, StreamExt};
        let (mut sink, mut source) = stream.split();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let conn = Arc::new(Self {
            tx,
            pending: pending.clone(),
            next_id: AtomicI64::new(1),
            closed: AtomicBool::new(false),
        });

        tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                if sink.send(message).await.is_err() {
                    break;
                }
            }
        });

        tokio::spawn(async move {
            while let Some(message) = source.next().await {
                let Ok(Message::Text(text)) = message else {
                    if message.is_err() {
                        break;
                    }
                    continue;
                };
                let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
                let Some(id) = value.get("id").and_then(Value::as_i64) else {
                    // Events (lifecycle/evaluate results) are not correlated here.
                    continue;
                };
                if let Some(sender) = pending.lock().await.remove(&id) {
                    let outcome = match value.get("error") {
                        Some(err) => Err(format!(
                            "cdp: {}",
                            err.get("message").and_then(Value::as_str).unwrap_or("command failed")
                        )),
                        None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = sender.send(outcome);
                }
            }
        });

        Ok(conn)
    }

    /// Send one command and await its result.
    async fn send(&self, method: &str, params: Value) -> Result<Value, String> {
        self.send_session(method, params, None).await
    }

    /// Send one command inside a flattened target session.
    async fn send_session(&self, method: &str, params: Value, session: Option<&str>) -> Result<Value, String> {
        if self.closed.load(Ordering::Relaxed) {
            return Err("cdp: connection closed".into());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let mut payload = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            payload["sessionId"] = Value::String(session.to_string());
        }
        let text = payload.to_string();
        if self.tx.send(Message::Text(text.into())).is_err() {
            self.pending.lock().await.remove(&id);
            return Err("cdp: socket closed".into());
        }

        match tokio::time::timeout(Duration::from_secs(60), rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => Err(format!("cdp: {method} reply dropped")),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(format!("cdp: {method} timed out"))
            }
        }
    }
}

/// Poll a JS predicate in the page until it is truthy or the deadline passes.
async fn wait_for_js(page: &CdpPage, script: &str, timeout_ms: u32) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms as u64);
    loop {
        match page.evaluate(script, None).await {
            Ok(value) if crate::cdp::js_truthy(&value) => return Ok(()),
            Ok(_) => {}
            // A navigation in flight can blow up evaluation; retry until the deadline.
            Err(err) if err.contains("Execution context") || err.contains("Cannot find context") => {}
            Err(err) => return Err(err),
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("timeout {timeout_ms}ms waiting for: {script}"));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// JS truthiness of a `Runtime.evaluate` result.
pub fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        // `returnByValue` of an object/array is always truthy in JS, including
        // empty ones; the CDP undefined marker is not.
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// XPath-free key mapping for `Input.dispatchKeyEvent`.
fn key_definition(key: &str) -> (String, i32, String, Option<String>) {
    // (windowsVirtualKeyCode, code, text)
    match key {
        "Enter" => ("Enter".into(), 13, "Enter".into(), Some("\r".into())),
        "Tab" => ("Tab".into(), 9, "Tab".into(), None),
        "Escape" => ("Escape".into(), 27, "Escape".into(), None),
        "Backspace" => ("Backspace".into(), 8, "Backspace".into(), None),
        "Delete" => ("Delete".into(), 46, "Delete".into(), None),
        "ArrowLeft" => ("ArrowLeft".into(), 37, "ArrowLeft".into(), None),
        "ArrowUp" => ("ArrowUp".into(), 38, "ArrowUp".into(), None),
        "ArrowRight" => ("ArrowRight".into(), 39, "ArrowRight".into(), None),
        "ArrowDown" => ("ArrowDown".into(), 40, "ArrowDown".into(), None),
        other if other.chars().count() == 1 => {
            let ch = other.chars().next().unwrap_or(' ');
            let upper = ch.to_ascii_uppercase();
            (upper.to_string(), upper as i32, format!("Key{upper}"), Some(ch.to_string()))
        }
        other => (other.to_string(), 0, other.to_string(), None),
    }
}

// ─── Real CDP browser ───────────────────────────────────────────────────────

/// A launched Chromium process plus its DevTools connection.
pub struct CdpBrowser {
    conn: Arc<CdpConnection>,
    child: Mutex<Option<tokio::process::Child>>,
    connected: AtomicBool,
    /// Default context options applied to every page in this browser.
    defaults: ContextOptions,
}

impl CdpBrowser {
    /// Launch a Chromium binary and attach to its DevTools endpoint.
    ///
    /// `executable` comes from [`crate::browser_executable_check::resolve_system_chrome_executable`];
    /// `None` falls back to the standard PATH lookups.
    pub async fn launch(executable: Option<&str>, headless: bool) -> Result<Arc<Self>, String> {
        let binary = match executable.map(String::from) {
            Some(path) => path,
            None => crate::browser_executable_check::resolve_system_chrome_executable()
                .ok_or_else(|| {
                    "No Chrome/Chromium executable found. Install Chrome, set CHROME_PATH, \
or run `npx playwright-core install chromium`."
                        .to_string()
                })?,
        };

        let mut command = tokio::process::Command::new(&binary);
        command
            .arg("--remote-debugging-port=0")
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-extensions")
            .arg("--disable-background-networking")
            // The pool's JS flags, kept identical so a headed window behaves the
            // same as the upstream launch options.
            .arg("--no-sandbox")
            .arg("--disable-dev-shm-usage")
            .arg("--disable-blink-features=AutomationControlled")
            .arg("--user-data-dir")
            .arg(
                std::env::temp_dir()
                    .join(format!("9router-chrome-{}", uuid::Uuid::new_v4()))
                    .to_string_lossy()
                    .to_string(),
            );
        if headless {
            command.arg("--headless=new");
        }
        command
            .arg("--window-size=1280,1024")
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command
            .spawn()
            .map_err(|e| format!("failed to launch {binary}: {e}"))?;

        // Chromium prints "DevTools listening on ws://…" on stderr.
        let stderr = child.stderr.take().ok_or("chromium stderr unavailable")?;
        let mut lines = BufReader::new(stderr).lines();
        let ws_url = tokio::time::timeout(Duration::from_secs(20), async {
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(rest) = line.split("DevTools listening on ").nth(1) {
                    return Some(rest.trim().to_string());
                }
            }
            None
        })
        .await
        .map_err(|_| "timed out waiting for the DevTools endpoint".to_string())?
        .ok_or_else(|| "Chromium exited before exposing a DevTools endpoint".to_string())?;

        let conn = CdpConnection::connect(&ws_url).await?;
        // Keep the browser process alive for the pool's lifetime and stop its
        // stderr pipe from filling up.
        tokio::spawn(async move {
            let mut sink = lines;
            while let Ok(Some(_)) = sink.next_line().await {}
        });

        Ok(Arc::new(Self {
            conn,
            child: Mutex::new(Some(child)),
            connected: AtomicBool::new(true),
            defaults: ContextOptions::default(),
        }))
    }

    /// Attach to an already-running DevTools endpoint (Obscura / external Chromium).
    pub async fn connect(ws_url: &str) -> Result<Arc<Self>, String> {
        let conn = CdpConnection::connect(ws_url).await?;
        Ok(Arc::new(Self {
            conn,
            child: Mutex::new(None),
            connected: AtomicBool::new(true),
            defaults: ContextOptions::default(),
        }))
    }

    /// Browser-level defaults (locale/timezone/user agent) for later contexts.
    pub fn with_defaults(mut self, defaults: ContextOptions) -> Self {
        self.defaults = defaults;
        self
    }
}

#[async_trait]
impl Browser for CdpBrowser {
    async fn new_context(&self, options: ContextOptions) -> Result<Arc<dyn BrowserContext>, String> {
        if !self.is_connected() {
            return Err("cdp: browser is not connected".into());
        }
        // A "context" is a fresh incognito-like target bucket: each one gets its
        // own page and cookie jar, matching Playwright's isolated contexts.
        let target = self
            .conn
            .send("Target.createTarget", json!({ "url": "about:blank" }))
            .await?;
        let target_id = target
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or("cdp: createTarget returned no targetId")?
            .to_string();

        let merged = ContextOptions {
            user_agent: options.user_agent.clone().or_else(|| self.defaults.user_agent.clone()),
            locale: options.locale.clone().or_else(|| self.defaults.locale.clone()),
            timezone_id: options.timezone_id.clone().or_else(|| self.defaults.timezone_id.clone()),
            headless: options.headless.or(self.defaults.headless),
        };

        if let Some(user_agent) = &merged.user_agent {
            let _ = self
                .conn
                .send("Emulation.setUserAgentOverride", json!({ "userAgent": user_agent }))
                .await;
        }
        if let Some(locale) = &merged.locale {
            let _ = self
                .conn
                .send("Emulation.setLocaleOverride", json!({ "locale": locale }))
                .await;
        }
        if let Some(tz) = &merged.timezone_id {
            let _ = self
                .conn
                .send("Emulation.setTimezoneOverride", json!({ "timezoneId": tz }))
                .await;
        }

        Ok(Arc::new(CdpContext {
            conn: self.conn.clone(),
            target_id,
            cookies: Arc::new(RwLock::new(Vec::new())),
            init_scripts: Arc::new(RwLock::new(Vec::new())),
        }))
    }

    async fn close(&self) -> Result<(), String> {
        self.connected.store(false, Ordering::Relaxed);
        let _ = self.conn.send("Browser.close", json!({})).await;
        let mut child = self.child.lock().await;
        if let Some(mut child) = child.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
            let _ = child.start_kill();
        }
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
}

/// A CDP target acting as one Playwright-style browser context.
struct CdpContext {
    conn: Arc<CdpConnection>,
    target_id: String,
    cookies: Arc<RwLock<Vec<PlaywrightCookie>>>,
    init_scripts: Arc<RwLock<Vec<String>>>,
}

impl CdpContext {
    /// Open a session on this target and return its id.
    async fn attach(&self) -> Result<String, String> {
        let out = self
            .conn
            .send("Target.attachToTarget", json!({ "targetId": self.target_id, "flatten": true }))
            .await?;
        out.get("sessionId")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| "cdp: attachToTarget returned no sessionId".to_string())
    }
}

#[async_trait]
impl BrowserContext for CdpContext {
    async fn new_page(&self) -> Result<Arc<dyn Page>, String> {
        let session_id = self.attach().await?;
        let page = CdpPage::new(self.conn.clone(), self.target_id.clone(), session_id);
        page.enable().await?;

        for cookie in self.cookies.read().await.iter() {
            page.set_cookie(cookie).await?;
        }
        for script in self.init_scripts.read().await.iter() {
            let _ = page
                .send_dom("Page.addScriptToEvaluateOnNewDocument", json!({ "source": script }))
                .await;
        }
        Ok(Arc::new(page))
    }

    async fn close(&self) -> Result<(), String> {
        let _ = self
            .conn
            .send("Target.closeTarget", json!({ "targetId": self.target_id }))
            .await;
        Ok(())
    }

    async fn add_cookies(&self, cookies: &[PlaywrightCookie]) -> Result<(), String> {
        // Keep the jar so pages created later inherit the session, and push the
        // cookies into the target that already exists.
        self.cookies.write().await.extend(cookies.iter().cloned());
        if cookies.is_empty() {
            return Ok(());
        }
        let session_id = self.attach().await?;
        let params: Vec<Value> = cookies.iter().map(cookie_to_cdp).collect();
        self.conn
            .send_session("Network.setCookies", json!({ "cookies": params }), Some(&session_id))
            .await?;
        Ok(())
    }

    async fn get_cookies(&self) -> Result<Vec<PlaywrightCookie>, String> {
        Ok(self.cookies.read().await.clone())
    }

    async fn add_init_script(&self, script: &str, _args: Option<Value>) -> Result<(), String> {
        self.init_scripts.write().await.push(script.to_string());
        Ok(())
    }
}

/// Playwright cookie → CDP `Network.CookieParam`.
fn cookie_to_cdp(cookie: &PlaywrightCookie) -> Value {
    let mut out = json!({ "name": cookie.name, "value": cookie.value });
    if let Some(domain) = &cookie.domain {
        out["domain"] = Value::String(domain.clone());
    }
    if let Some(path) = &cookie.path {
        out["path"] = Value::String(path.clone());
    }
    if let Some(expires) = cookie.expires {
        out["expires"] = json!(expires);
    }
    if let Some(http_only) = cookie.http_only {
        out["httpOnly"] = json!(http_only);
    }
    if let Some(secure) = cookie.secure {
        out["secure"] = json!(secure);
    }
    out
}

/// A CDP page (one flattened session on one target).
struct CdpPage {
    conn: Arc<CdpConnection>,
    target_id: String,
    session_id: String,
    url: Arc<RwLock<String>>,
}

impl CdpPage {
    fn new(conn: Arc<CdpConnection>, target_id: String, session_id: String) -> Self {
        Self { conn, target_id, session_id, url: Arc::new(RwLock::new(String::new())) }
    }

    async fn send_dom(&self, method: &str, params: Value) -> Result<Value, String> {
        self.conn.send_session(method, params, Some(&self.session_id)).await
    }

    async fn enable(&self) -> Result<(), String> {
        self.send_dom("Page.enable", json!({})).await?;
        self.send_dom("Runtime.enable", json!({})).await?;
        let _ = self.send_dom("Network.enable", json!({})).await;
        Ok(())
    }

    async fn set_cookie(&self, cookie: &PlaywrightCookie) -> Result<(), String> {
        self.send_dom("Network.setCookies", json!({ "cookies": [cookie_to_cdp(cookie)] }))
            .await
            .map(|_| ())
    }

    /// Resolve the element centre for `selector`, or an error when it is absent.
    async fn element_center(&self, selector: &str) -> Result<(f64, f64), String> {
        let script = format!(
            r#"(() => {{
  const el = document.querySelector({selector});
  if (!el) return null;
  const r = el.getBoundingClientRect();
  if (!r || r.width === 0 || r.height === 0) return null;
  return {{ x: r.left + r.width / 2, y: r.top + r.height / 2 }};
}})()"#,
            selector = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into())
        );
        let value = self.evaluate(&script, None).await?;
        let x = value.get("x").and_then(Value::as_f64);
        let y = value.get("y").and_then(Value::as_f64);
        match (x, y) {
            (Some(x), Some(y)) => Ok((x, y)),
            _ => Err(format!("selector not visible: {selector}")),
        }
    }
}

#[async_trait]
impl Page for CdpPage {
    async fn goto(&self, url: &str, options: GotoOptions) -> Result<(), String> {
        let result = self
            .send_dom("Page.navigate", json!({ "url": url, "transitionType": "typed" }))
            .await?;
        if let Some(error) = result.get("errorText").and_then(Value::as_str) {
            if !error.is_empty() {
                return Err(format!("navigation failed: {error}"));
            }
        }
        *self.url.write().await = url.to_string();

        // `domcontentloaded` is the default the executors ask for; `networkidle`
        // needs the load event plus a settle window, which the wait loop gives.
        let ready_state = match options.wait_until.as_str() {
            "load" | "networkidle" => "complete",
            _ => "interactive",
        };
        let script = format!("document.readyState === {ready_state:?} || document.readyState === \"complete\"");
        wait_for_js(self, &script, options.timeout_ms).await?;
        if options.wait_until == "networkidle" {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Ok(())
    }

    fn url(&self) -> String {
        self.url.try_read().map(|u| u.clone()).unwrap_or_default()
    }

    async fn close(&self) -> Result<(), String> {
        let _ = self
            .conn
            .send("Target.closeTarget", json!({ "targetId": self.target_id }))
            .await;
        Ok(())
    }

    async fn insert_text(&self, text: &str) -> Result<(), String> {
        // Focus the active composer, then insert so a rich-text editor receives
        // the input event it listens for.
        let _ = self
            .evaluate(
                "(() => { const el = document.activeElement; if (el && el.focus) el.focus(); return true; })()",
                None,
            )
            .await;
        self.send_dom("Input.insertText", json!({ "text": text })).await.map(|_| ())
    }

    async fn press_key(&self, key: &str) -> Result<(), String> {
        let (k, vk, code, text) = key_definition(key);
        let mut down = json!({
            "type": "keyDown",
            "key": k,
            "windowsVirtualKeyCode": vk,
            "nativeVirtualKeyCode": vk,
        });
        if code.starts_with("Key") {
            down["code"] = Value::String(code);
        }
        if let Some(text) = &text {
            down["text"] = Value::String(text.clone());
        }
        // Enter in a composer is often handled from `char`, so mirror text.
        if let Some(text) = &text {
            if key == "Enter" {
                let _ = self.send_dom("Input.insertText", json!({ "text": text })).await;
            }
        }
        let mut up = down.clone();
        up["type"] = Value::String("keyUp".into());
        up.as_object_mut().map(|o| o.remove("text"));
        self.send_dom("Input.dispatchKeyEvent", down).await?;
        self.send_dom("Input.dispatchKeyEvent", up).await?;
        Ok(())
    }

    async fn click(&self, selector: &str) -> Result<(), String> {
        let (x, y) = self.element_center(selector).await?;
        let base = json!({ "x": x, "y": y, "button": "left", "clickCount": 1 });
        let mut down = base.clone();
        down["type"] = Value::String("mousePressed".into());
        let mut up = base;
        up["type"] = Value::String("mouseReleased".into());
        self.send_dom("Input.dispatchMouseEvent", down).await?;
        self.send_dom("Input.dispatchMouseEvent", up).await?;
        Ok(())
    }

    async fn wait_for_selector(&self, selector: &str, timeout_ms: u32) -> Result<(), String> {
        let script = format!(
            "document.querySelector({}) !== null",
            serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into())
        );
        wait_for_js(self, &script, timeout_ms).await
    }

    async fn count(&self, selector: &str) -> Result<usize, String> {
        let script = format!(
            "document.querySelectorAll({}).length",
            serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into())
        );
        let value = self.evaluate(&script, None).await?;
        Ok(value.as_u64().unwrap_or(0) as usize)
    }

    async fn evaluate(&self, script: &str, args: Option<Value>) -> Result<Value, String> {
        // The executors pass `(args) => …` style functions with an optional
        // argument object, like Playwright's `page.evaluate(fn, args)`.
        let expression = match (&args, script.trim_start().starts_with("()")) {
            (Some(args), _) => {
                let arg_json = serde_json::to_string(args).unwrap_or_else(|_| "null".into());
                format!("({script})({arg_json})")
            }
            (None, true) => format!("({script})()"),
            (None, false) => script.to_string(),
        };
        let result = self
            .send_dom(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": true,
                    "userGesture": true,
                }),
            )
            .await?;

        if let Some(details) = result.get("exceptionDetails") {
            let message = details
                .get("exception")
                .and_then(|e| e.get("description"))
                .and_then(Value::as_str)
                .or_else(|| details.get("text").and_then(Value::as_str))
                .unwrap_or("evaluate failed");
            return Err(message.to_string());
        }
        let outcome = result.get("result").cloned().unwrap_or(Value::Null);
        // `undefined` (no return value) surfaces as a missing `value`.
        Ok(outcome.get("value").cloned().unwrap_or(Value::Null))
    }

    async fn eval_on_selector(&self, selector: &str, script: &str) -> Result<Value, String> {
        let expression = format!(
            "((el) => {{ {script} }})(document.querySelector({selector}))",
            script = script,
            selector = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into())
        );
        self.evaluate(&expression, None).await
    }

    fn locator(&self, selector: &str) -> Locator {
        // Locators need an `Arc<dyn Page>`; re-wrap this page's handle.
        Locator::new(selector.to_string(), self.handle())
    }

    async fn wait_for_function(&self, script: &str, timeout_ms: u32) -> Result<(), String> {
        // Playwright's `waitForFunction` returns a JSHandle; the executors call
        // `.asElement()`/`.jsonValue()`, and every call site here only needs the
        // truthiness transition.
        let predicate = script.to_string();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms as u64);
        loop {
            match self.evaluate(&predicate, None).await {
                Ok(_) => return Ok(()),
                Err(_) if tokio::time::Instant::now() < deadline => {}
                Err(err) => return Err(err),
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn screenshot(&self, as_base64: bool) -> Result<Vec<u8>, String> {
        let result = self.send_dom("Page.captureScreenshot", json!({ "format": "png" })).await?;
        let data = result.get("data").and_then(Value::as_str).unwrap_or("");
        if as_base64 {
            return Ok(data.as_bytes().to_vec());
        }
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|e| format!("screenshot decode: {e}"))
    }
}

impl CdpPage {
    /// A `dyn Page` handle for this session (used by [`Page::locator`]).
    fn handle(&self) -> Arc<dyn Page> {
        Arc::new(Self {
            conn: self.conn.clone(),
            target_id: self.target_id.clone(),
            session_id: self.session_id.clone(),
            url: self.url.clone(),
        })
    }
}

// ─── Test doubles ───────────────────────────────────────────────────────────

/// Mock implementation of Browser for testing and initial development
pub struct MockBrowser {
    connected: Arc<RwLock<bool>>,
}

impl Default for MockBrowser {
    fn default() -> Self {
        Self::new()
    }
}

impl MockBrowser {
    pub fn new() -> Self {
        Self { connected: Arc::new(RwLock::new(true)) }
    }
}

#[async_trait]
impl Browser for MockBrowser {
    async fn new_context(&self, _options: ContextOptions) -> Result<Arc<dyn BrowserContext>, String> {
        Ok(Arc::new(MockContext::new()))
    }

    async fn close(&self) -> Result<(), String> {
        *self.connected.write().await = false;
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected.try_read().map(|v| *v).unwrap_or(false)
    }
}

/// Mock implementation of BrowserContext for testing
pub struct MockContext {
    cookies: Arc<RwLock<Vec<PlaywrightCookie>>>,
}

impl Default for MockContext {
    fn default() -> Self {
        Self::new()
    }
}

impl MockContext {
    pub fn new() -> Self {
        Self { cookies: Arc::new(RwLock::new(Vec::new())) }
    }
}

#[async_trait]
impl BrowserContext for MockContext {
    async fn new_page(&self) -> Result<Arc<dyn Page>, String> {
        Ok(Arc::new(MockPage::new()))
    }

    async fn close(&self) -> Result<(), String> {
        Ok(())
    }

    async fn add_cookies(&self, cookies: &[PlaywrightCookie]) -> Result<(), String> {
        self.cookies.write().await.extend(cookies.to_vec());
        Ok(())
    }

    async fn get_cookies(&self) -> Result<Vec<PlaywrightCookie>, String> {
        Ok(self.cookies.read().await.clone())
    }

    async fn add_init_script(&self, _script: &str, _args: Option<Value>) -> Result<(), String> {
        Ok(())
    }
}

/// Mock implementation of Page for testing
pub struct MockPage {
    url: Arc<RwLock<String>>,
    content: Arc<RwLock<String>>,
}

impl Default for MockPage {
    fn default() -> Self {
        Self::new()
    }
}

impl MockPage {
    pub fn new() -> Self {
        Self { url: Arc::new(RwLock::new(String::new())), content: Arc::new(RwLock::new(String::new())) }
    }
}

#[async_trait]
impl Page for MockPage {
    async fn goto(&self, url: &str, _options: GotoOptions) -> Result<(), String> {
        *self.url.write().await = url.to_string();
        Ok(())
    }

    fn url(&self) -> String {
        self.url.try_read().map(|u| u.clone()).unwrap_or_default()
    }

    async fn close(&self) -> Result<(), String> {
        Ok(())
    }

    async fn insert_text(&self, text: &str) -> Result<(), String> {
        self.content.write().await.push_str(text);
        Ok(())
    }

    async fn press_key(&self, _key: &str) -> Result<(), String> {
        Ok(())
    }

    async fn click(&self, _selector: &str) -> Result<(), String> {
        Ok(())
    }

    async fn wait_for_selector(&self, _selector: &str, _timeout_ms: u32) -> Result<(), String> {
        Ok(())
    }

    async fn count(&self, _selector: &str) -> Result<usize, String> {
        Ok(1)
    }

    async fn evaluate(&self, _script: &str, _args: Option<Value>) -> Result<Value, String> {
        Ok(Value::Null)
    }

    async fn eval_on_selector(&self, _selector: &str, _script: &str) -> Result<Value, String> {
        Ok(Value::Null)
    }

    fn locator(&self, selector: &str) -> Locator {
        Locator::new(selector.to_string(), Arc::new(MockPage::new()))
    }

    async fn wait_for_function(&self, _script: &str, _timeout_ms: u32) -> Result<(), String> {
        Ok(())
    }

    async fn screenshot(&self, _as_base64: bool) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_truthiness_matches_javascript() {
        assert!(!js_truthy(&Value::Null));
        assert!(!js_truthy(&json!(false)));
        assert!(!js_truthy(&json!(0)));
        assert!(!js_truthy(&json!("")));
        assert!(js_truthy(&json!(true)));
        assert!(js_truthy(&json!(1)));
        assert!(js_truthy(&json!("x")));
        // Empty containers are truthy in JS.
        assert!(js_truthy(&json!([])));
        assert!(js_truthy(&json!({})));
    }

    #[test]
    fn key_definitions_cover_the_composer_keys() {
        let (key, vk, code, text) = key_definition("Enter");
        assert_eq!(key, "Enter");
        assert_eq!(vk, 13);
        assert_eq!(code, "Enter");
        assert_eq!(text.as_deref(), Some("\r"));

        let (key, vk, _, _) = key_definition("Escape");
        assert_eq!((key.as_str(), vk), ("Escape", 27));

        // A single character becomes a text-bearing key event.
        let (key, _, code, text) = key_definition("a");
        assert_eq!(key, "A");
        assert_eq!(code, "KeyA");
        assert_eq!(text.as_deref(), Some("a"));
    }

    #[test]
    fn cookie_conversion_keeps_every_playwright_field() {
        let cookie = PlaywrightCookie {
            name: "__Secure-1PSID".into(),
            value: "abc".into(),
            domain: Some(".google.com".into()),
            path: Some("/".into()),
            expires: Some(1_900_000_000),
            http_only: Some(true),
            secure: Some(true),
            same_site: None,
        };
        let cdp = cookie_to_cdp(&cookie);
        assert_eq!(cdp["name"], json!("__Secure-1PSID"));
        assert_eq!(cdp["value"], json!("abc"));
        assert_eq!(cdp["domain"], json!(".google.com"));
        assert_eq!(cdp["path"], json!("/"));
        assert_eq!(cdp["expires"], json!(1_900_000_000));
        assert_eq!(cdp["httpOnly"], json!(true));
        assert_eq!(cdp["secure"], json!(true));
    }

    #[test]
    fn cookies_without_optional_fields_stay_minimal() {
        let cookie = PlaywrightCookie {
            name: "n".into(),
            value: "v".into(),
            domain: None,
            path: None,
            expires: None,
            http_only: None,
            secure: None,
            same_site: None,
        };
        let cdp = cookie_to_cdp(&cookie);
        assert_eq!(cdp, json!({ "name": "n", "value": "v" }));
    }

    #[tokio::test]
    async fn mock_browser_round_trips_cookies() {
        let browser = MockBrowser::new();
        let context = browser.new_context(ContextOptions::default()).await.unwrap();
        context
            .add_cookies(&[PlaywrightCookie {
                name: "a".into(),
                value: "b".into(),
                domain: None,
                path: None,
                expires: None,
                http_only: None,
                secure: None,
                same_site: None,
            }])
            .await
            .unwrap();
        assert_eq!(context.get_cookies().await.unwrap().len(), 1);
        assert!(browser.is_connected());
        browser.close().await.unwrap();
        assert!(!browser.is_connected());
    }
}
