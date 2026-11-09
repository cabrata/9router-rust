//! HTTP client for the local gateway's `/api` surface, used by the interactive
//! menus. Port of `cli/src/cli/api/client.js`.
//!
//! Every call returns `ApiResult` — `{ ok: true, data }` or `{ ok: false, error }`
//! — mirroring the JS `{ success, data/error }` envelope. Requests carry the
//! `x-9r-cli-token` header so the dashboard guard accepts them without a login
//! cookie; the token is `sha256(machineId + "9r-cli-auth" + cliSecret)` truncated
//! to 16 hex chars, exactly as `src/shared/utils/machineId.js` computes it.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use nr_core::paths;

/// Header the dashboard guard looks for.
pub const CLI_TOKEN_HEADER: &str = "x-9r-cli-token";
/// Salt for the CLI token (`CLI_TOKEN_SALT` in dashboardGuard.js).
pub const CLI_TOKEN_SALT: &str = "9r-cli-auth";

fn auth_dir() -> PathBuf {
    paths::data_dir().join("auth")
}

fn cli_secret_file() -> PathBuf {
    auth_dir().join("cli-secret")
}

fn machine_id_file() -> PathBuf {
    paths::data_dir().join("machine-id")
}

/// Raw machine id, reading the shared file the server writes first
/// (`loadRawMachineId` in machineId.js).
fn load_raw_machine_id() -> String {
    if let Ok(raw) = std::fs::read_to_string(machine_id_file()) {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    // Fall back to the platform machine id, then a random UUID; persist it so
    // the server and the CLI agree on the value.
    let generated = platform_machine_id().unwrap_or_else(|| random_hex(16));
    let _ = std::fs::create_dir_all(paths::data_dir());
    let _ = std::fs::write(machine_id_file(), &generated);
    generated
}

fn platform_machine_id() -> Option<String> {
    for p in ["/etc/machine-id", "/var/lib/dbus/machine-id", "/var/db/dbus/machine-id"] {
        if let Ok(s) = std::fs::read_to_string(p) {
            let t = s.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    std::env::var("HOSTNAME").ok().filter(|h| !h.is_empty())
}

/// Random secret shared with the server through a file, so the token is not
/// derivable from the machine id alone (`loadCliSecret`).
fn load_cli_secret() -> String {
    if let Ok(secret) = std::fs::read_to_string(cli_secret_file()) {
        let trimmed = secret.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    let secret = random_hex(32);
    let _ = std::fs::create_dir_all(auth_dir());
    let _ = std::fs::write(cli_secret_file(), &secret);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(cli_secret_file(), std::fs::Permissions::from_mode(0o600));
    }
    secret
}

fn random_hex(bytes: usize) -> String {
    // 32 random bytes from the OS; no extra crate needed.
    let mut buf = vec![0u8; bytes];
    let filled = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| {
            use std::io::Read;
            f.read_exact(&mut buf)
        })
        .is_ok();
    if !filled {
        // Deterministic fallback: pid + time. Never used on a normal Linux box.
        let seed = format!("{}{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        buf = seed.into_bytes();
        buf.resize(bytes.max(1), 0);
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// The `x-9r-cli-token` value. Cached for the process.
pub fn cli_token() -> &'static str {
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN.get_or_init(|| {
        let raw = load_raw_machine_id();
        if raw.is_empty() {
            return String::new();
        }
        compute_cli_token(&raw, &load_cli_secret())
    })
}

/// `sha256(raw + salt + secret)[..16]` — the formula both sides agree on.
pub fn compute_cli_token(raw_machine_id: &str, secret: &str) -> String {
    if raw_machine_id.is_empty() {
        return String::new();
    }
    let mut hasher = Sha256::new();
    hasher.update(format!("{raw_machine_id}{CLI_TOKEN_SALT}{secret}").as_bytes());
    let hex = format!("{:x}", hasher.finalize());
    hex.chars().take(16).collect()
}

/// `{ success, data | error }` — the envelope every JS API call returns.
#[derive(Debug, Clone)]
pub struct ApiResult {
    pub ok: bool,
    pub data: Value,
    pub error: Option<String>,
    pub status: u16,
    /// Present on poll responses: `pending` / `slow_down` mean "keep going".
    pub pending: bool,
}

impl ApiResult {
    pub fn ok(data: Value) -> Self {
        Self { ok: true, data, error: None, status: 200, pending: false }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Self { ok: false, data: Value::Null, error: Some(msg.into()), status: 0, pending: false }
    }
    pub fn error_text(&self) -> &str {
        self.error.as_deref().unwrap_or("unknown error")
    }
    /// `data.<key>` with a null fallback.
    pub fn get(&self, key: &str) -> &Value {
        static NULL: Value = Value::Null;
        self.data.get(key).unwrap_or(&NULL)
    }
    pub fn array(&self, key: &str) -> Vec<Value> {
        self.get(key).as_array().cloned().unwrap_or_default()
    }
    /// `data` when it is already the array (some routes return a bare list).
    pub fn as_array(&self) -> Vec<Value> {
        self.data.as_array().cloned().unwrap_or_default()
    }
}

/// Base URL + credentials for the API client (`api.configure({ port })`).
#[derive(Debug, Clone)]
pub struct ApiClient {
    host: String,
    port: u16,
    scheme: String,
    pub timeout: Duration,
}

impl ApiClient {
    pub fn new(port: u16) -> Self {
        Self { host: "localhost".into(), port, scheme: "http".into(), timeout: Duration::from_secs(30) }
    }

    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = host.into();
        self
    }

    pub fn base_url(&self) -> String {
        format!("{}://{}:{}", self.scheme, self.host, self.port)
    }

    fn request(&self, method: &str, path: &str, body: Option<&Value>) -> ApiResult {
        let client = match reqwest::blocking::Client::builder().timeout(self.timeout).build() {
            Ok(c) => c,
            Err(e) => return ApiResult::err(format!("Network error: {e}")),
        };
        let url = format!("{}{}", self.base_url(), path);
        let m = reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
        let mut req = client
            .request(m, &url)
            .header("Content-Type", "application/json")
            .header(CLI_TOKEN_HEADER, cli_token());
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = match req.send() {
            Ok(r) => r,
            Err(e) => return ApiResult::err(format!("Network error: {e}")),
        };
        let status = resp.status().as_u16();
        let text = resp.text().unwrap_or_default();
        let parsed: Value = if text.trim().is_empty() {
            json!({})
        } else {
            match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => return ApiResult::err(format!("Failed to parse response: {e}")),
            }
        };
        let error = parsed.get("error").and_then(Value::as_str).filter(|s| !s.is_empty());
        if status >= 400 || error.is_some() {
            return ApiResult {
                ok: false,
                data: parsed.clone(),
                error: Some(error.map(str::to_string).unwrap_or_else(|| format!("HTTP {status}"))),
                status,
                pending: parse_pending(&parsed),
            };
        }
        let pending = parse_pending(&parsed);
        ApiResult { ok: true, data: parsed, error: None, status, pending }
    }

    fn get(&self, path: &str) -> ApiResult {
        self.request("GET", path, None)
    }
    fn post(&self, path: &str, body: Option<&Value>) -> ApiResult {
        self.request("POST", path, body)
    }
    fn put(&self, path: &str, body: Option<&Value>) -> ApiResult {
        self.request("PUT", path, body)
    }
    fn patch(&self, path: &str, body: Option<&Value>) -> ApiResult {
        self.request("PATCH", path, body)
    }
    fn delete(&self, path: &str) -> ApiResult {
        self.request("DELETE", path, None)
    }

    // ── providers ───────────────────────────────────────────────────────────

    pub fn get_providers(&self) -> ApiResult {
        self.get("/api/providers")
    }
    pub fn test_provider(&self, id: &str) -> ApiResult {
        self.post(&format!("/api/providers/{}/test", enc(id)), None)
    }
    pub fn delete_provider(&self, id: &str) -> ApiResult {
        self.delete(&format!("/api/providers/{}", enc(id)))
    }
    pub fn update_connection(&self, id: &str, data: &Value) -> ApiResult {
        self.put(&format!("/api/providers/{}", enc(id)), Some(data))
    }
    pub fn create_api_key_provider(&self, data: &Value) -> ApiResult {
        self.post("/api/providers", Some(data))
    }

    // ── oauth ───────────────────────────────────────────────────────────────

    pub fn get_oauth_auth_url(&self, provider: &str) -> ApiResult {
        // Codex requires fixed port 1455 and path /auth/callback.
        let redirect = if provider == "codex" {
            "http://localhost:1455/auth/callback"
        } else {
            "http://localhost:20128/callback"
        };
        self.get(&format!("/api/oauth/{}/authorize?redirect_uri={}", enc(provider), enc(redirect)))
    }
    pub fn exchange_oauth_code(&self, provider: &str, data: &Value) -> ApiResult {
        self.post(&format!("/api/oauth/{}/exchange", enc(provider)), Some(data))
    }
    pub fn get_oauth_device_code(&self, provider: &str) -> ApiResult {
        self.get(&format!("/api/oauth/{}/device-code", enc(provider)))
    }
    pub fn poll_oauth_token(&self, provider: &str, data: &Value) -> ApiResult {
        self.post(&format!("/api/oauth/{}/poll", enc(provider)), Some(data))
    }

    // ── api keys ────────────────────────────────────────────────────────────

    pub fn get_api_keys(&self) -> ApiResult {
        self.get("/api/keys")
    }
    pub fn create_api_key(&self, name: &str) -> ApiResult {
        self.post("/api/keys", Some(&json!({ "name": name })))
    }
    pub fn delete_api_key(&self, id: &str) -> ApiResult {
        self.delete(&format!("/api/keys/{}", enc(id)))
    }

    // ── combos ──────────────────────────────────────────────────────────────

    pub fn get_combos(&self) -> ApiResult {
        self.get("/api/combos")
    }
    pub fn get_combo_by_id(&self, id: &str) -> ApiResult {
        self.get(&format!("/api/combos/{}", enc(id)))
    }
    pub fn create_combo(&self, data: &Value) -> ApiResult {
        self.post("/api/combos", Some(data))
    }
    pub fn update_combo(&self, id: &str, data: &Value) -> ApiResult {
        self.put(&format!("/api/combos/{}", enc(id)), Some(data))
    }
    pub fn delete_combo(&self, id: &str) -> ApiResult {
        self.delete(&format!("/api/combos/{}", enc(id)))
    }

    // ── cli tools ───────────────────────────────────────────────────────────

    pub fn get_cli_tool_settings(&self, tool: &str, profile: Option<&str>) -> ApiResult {
        let qs = profile.map(|p| format!("?profile={}", enc(p))).unwrap_or_default();
        self.get(&format!("/api/cli-tools/{tool}-settings{qs}"))
    }
    pub fn apply_cli_tool_settings(&self, tool: &str, body: &Value) -> ApiResult {
        self.post(&format!("/api/cli-tools/{tool}-settings"), Some(body))
    }
    pub fn reset_cli_tool_settings(&self, tool: &str, profile: Option<&str>) -> ApiResult {
        let qs = profile.map(|p| format!("?profile={}", enc(p))).unwrap_or_default();
        self.delete(&format!("/api/cli-tools/{tool}-settings{qs}"))
    }
    pub fn list_hermes_profiles(&self) -> ApiResult {
        self.get("/api/cli-tools/hermes-profiles")
    }

    // ── settings ────────────────────────────────────────────────────────────

    pub fn get_settings(&self) -> ApiResult {
        self.get("/api/settings")
    }
    pub fn update_settings(&self, data: &Value) -> ApiResult {
        self.patch("/api/settings", Some(data))
    }
    pub fn reset_password(&self) -> ApiResult {
        self.post("/api/auth/reset-password", None)
    }

    // ── models ──────────────────────────────────────────────────────────────

    pub fn get_models(&self) -> ApiResult {
        self.get("/api/models")
    }
    pub fn get_available_models(&self) -> ApiResult {
        self.get("/v1/models")
    }

    // ── provider nodes (custom providers) ───────────────────────────────────

    pub fn get_provider_nodes(&self) -> ApiResult {
        self.get("/api/provider-nodes")
    }
    pub fn create_provider_node(&self, data: &Value) -> ApiResult {
        self.post("/api/provider-nodes", Some(data))
    }
    pub fn update_provider_node(&self, id: &str, data: &Value) -> ApiResult {
        self.put(&format!("/api/provider-nodes/{}", enc(id)), Some(data))
    }
    pub fn delete_provider_node(&self, id: &str) -> ApiResult {
        self.delete(&format!("/api/provider-nodes/{}", enc(id)))
    }

    // ── tunnel ──────────────────────────────────────────────────────────────

    pub fn get_tunnel_status(&self) -> ApiResult {
        self.get("/api/tunnel/status")
    }
    pub fn enable_tunnel(&self) -> ApiResult {
        self.post("/api/tunnel/enable", None)
    }
    pub fn disable_tunnel(&self) -> ApiResult {
        self.post("/api/tunnel/disable", None)
    }

    // ── usage ───────────────────────────────────────────────────────────────

    pub fn get_usage_stats(&self, period: &str) -> ApiResult {
        self.get(&format!("/api/usage/stats?period={}", enc(period)))
    }
    pub fn get_usage_logs(&self) -> ApiResult {
        self.get("/api/usage/logs")
    }
    pub fn get_usage_providers(&self, period: &str) -> ApiResult {
        self.get(&format!("/api/usage/providers?period={}", enc(period)))
    }
}

fn parse_pending(parsed: &Value) -> bool {
    parsed.get("pending").and_then(Value::as_bool).unwrap_or(false)
}

/// Percent-encode a path segment or query value.
pub fn enc(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

/// Resolve the endpoint shown in menu headers: tunnel public URL when on,
/// else the local `/v1` URL (`utils/endpoint.js`).
pub fn endpoint(api: &ApiClient, port: u16) -> (String, bool) {
    let res = api.get_tunnel_status();
    if res.ok {
        // The status route nests the tunnel state under `tunnel`.
        let tunnel = res.get("tunnel");
        let enabled = tunnel.get("enabled").and_then(Value::as_bool).unwrap_or(false);
        let public_url = tunnel.get("publicUrl").and_then(Value::as_str).unwrap_or("");
        if enabled && !public_url.is_empty() {
            return (format!("{public_url}/v1"), true);
        }
    }
    (format!("http://localhost:{port}/v1"), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_token_is_16_lowercase_hex_chars() {
        let token = compute_cli_token("machine-1", "secret-1");
        assert_eq!(token.len(), 16, "token {token:?}");
        assert!(token.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn cli_token_uses_machine_id_salt_and_secret() {
        // Distinct inputs must not collide, and the empty machine id yields no token.
        assert_ne!(compute_cli_token("m1", "s1"), compute_cli_token("m2", "s1"));
        assert_ne!(compute_cli_token("m1", "s1"), compute_cli_token("m1", "s2"));
        assert_eq!(compute_cli_token("", "s1"), "");
        // Pin the formula so the JS server and this client cannot drift apart.
        assert_eq!(
            compute_cli_token("abc", "def"),
            {
                let mut h = Sha256::new();
                h.update(b"abc9r-cli-authdef");
                format!("{:x}", h.finalize()).chars().take(16).collect::<String>()
            }
        );
    }

    #[test]
    fn api_result_envelope_helpers() {
        let ok = ApiResult::ok(json!({"keys": [{"id": "k1"}]}));
        assert!(ok.ok);
        assert_eq!(ok.array("keys").len(), 1);
        assert_eq!(ok.get("missing"), &Value::Null);
        let err = ApiResult::err("boom");
        assert!(!err.ok);
        assert_eq!(err.error_text(), "boom");
    }

    #[test]
    fn base_url_uses_the_configured_port() {
        let api = ApiClient::new(20128);
        assert_eq!(api.base_url(), "http://localhost:20128");
        assert_eq!(api.with_host("10.0.0.2").base_url(), "http://10.0.0.2:20128");
    }

    #[test]
    fn enc_escapes_path_segments() {
        assert_eq!(enc("a b/c"), "a%20b%2Fc");
        assert_eq!(enc("plain-id"), "plain-id");
    }
}
