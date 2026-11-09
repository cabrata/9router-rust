//! Minimal Chrome DevTools Protocol (CDP) client for browser automation.
//! Supports Playwright-compatible context and page APIs.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

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
    /// Get on-page event stream (stub for now)
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

/// Options for page navigation
#[derive(Debug, Clone)]
pub struct GotoOptions {
    pub wait_until: String, // "load", "domcontentloaded", "networkidle"
    pub timeout_ms: u32,
}

impl Default for GotoOptions {
    fn default() -> Self {
        Self {
            wait_until: "domcontentloaded".to_string(),
            timeout_ms: 30000,
        }
    }
}

/// Mock implementation of Browser for testing and initial development
pub struct MockBrowser {
    connected: Arc<RwLock<bool>>,
}

impl MockBrowser {
    pub fn new() -> Self {
        Self {
            connected: Arc::new(RwLock::new(true)),
        }
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
        // Would need to be async-safe in real impl
        true
    }
}

/// Mock implementation of BrowserContext for testing
pub struct MockContext {
    cookies: Arc<RwLock<Vec<PlaywrightCookie>>>,
}

impl MockContext {
    pub fn new() -> Self {
        Self {
            cookies: Arc::new(RwLock::new(Vec::new())),
        }
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

impl MockPage {
    pub fn new() -> Self {
        Self {
            url: Arc::new(RwLock::new(String::new())),
            content: Arc::new(RwLock::new(String::new())),
        }
    }
}

#[async_trait]
impl Page for MockPage {
    async fn goto(&self, url: &str, _options: GotoOptions) -> Result<(), String> {
        *self.url.write().await = url.to_string();
        Ok(())
    }

    fn url(&self) -> String {
        // Can't use async here, return placeholder for mock
        "".to_string()
    }

    async fn close(&self) -> Result<(), String> {
        Ok(())
    }

    async fn insert_text(&self, _text: &str) -> Result<(), String> {
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
        Ok(vec![])
    }
}
