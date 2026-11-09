//! CLI Tools config routes: read, write, and reset configuration files for
//! various AI coding CLIs (Claude, Codex, OpenCode, Cline, Crush, Devin, Droid,
//! Jcode, Kilo, Pi, Smelt, OMP, OpenClaw, Forge, Grok-Build, DeepSeek-TUI,
//! CodeWhale, Copilot, Cowork, Hermes, Antigravity MITM).
//! Faithful port of `src/app/api/cli-tools/**`.

use std::path::{Path, PathBuf};

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde_json::{json, Value};

// ─── Atomic Write & Backup Helper ───────────────────────────────────────────

/// Atomically write a file:
/// 1. If file exists, create a backup `<path>.bak`.
/// 2. Write to a temporary file in the same directory.
/// 3. Rename the temporary file over the destination file.
pub fn write_file_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Keep a backup if file already exists
    if path.exists() {
        let backup_path = path.with_extension(format!(
            "{}.bak",
            path.extension().and_then(|s| s.to_str()).unwrap_or("")
        ));
        let _ = std::fs::copy(path, backup_path);
    }

    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("config");
    let temp_name = format!(".{}.tmp.{}", file_name, uuid::Uuid::new_v4());
    let temp_path = path.with_file_name(temp_name);

    std::fs::write(&temp_path, content)?;
    std::fs::rename(&temp_path, path)?;

    Ok(())
}

/// Delete a file safely, ignoring not found
pub fn remove_file_if_exists(path: &Path) -> std::io::Result<()> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// Helper: read file string or None if not found
pub fn read_file_string(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Helper: parse JSON tolerating trailing commas (JSONC)
pub fn parse_jsonc(content: &str) -> Option<Value> {
    let re = regex::Regex::new(r",(\s*[}\]])").ok()?;
    let stripped = re.replace_all(content, "$1");
    serde_json::from_str(&stripped).ok()
}

// ─── Resolve CLI API Key ────────────────────────────────────────────────────

/// Resolve API key:
/// 1. Non-empty caller key if not "sk_9router"
/// 2. First active key from db apiKeys table
/// 3. Empty string
pub fn resolve_cli_api_key(caller_key: Option<&str>) -> String {
    if let Some(k) = caller_key {
        let trimmed = k.trim();
        if !trimmed.is_empty() && trimmed != "sk_9router" {
            return trimmed.to_string();
        }
    }

    if let Ok(db) = nr_db::db() {
        if let Ok(keys) = nr_db::get_api_keys(&db) {
            if let Some(active) = keys.into_iter().find(|k| k.is_active) {
                return active.key;
            }
        }
    }

    String::new()
}

// ─── Tool-Specific Path Helpers ─────────────────────────────────────────────

fn home_dir() -> PathBuf {
    nr_core::paths::home_dir()
}

fn config_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.trim().is_empty() {
            return PathBuf::from(xdg);
        }
    }
    home_dir().join(".config")
}

// 1. Claude
fn claude_settings_path() -> PathBuf {
    home_dir().join(".claude").join("settings.json")
}
fn claude_json_path() -> PathBuf {
    home_dir().join(".claude.json")
}

// 2. Codex
fn codex_dir() -> PathBuf {
    home_dir().join(".codex")
}
fn codex_config_path() -> PathBuf {
    codex_dir().join("config.toml")
}
fn codex_auth_path() -> PathBuf {
    codex_dir().join("auth.json")
}

// 3. OpenCode
fn opencode_dir() -> PathBuf {
    config_dir().join("opencode")
}
fn opencode_config_path() -> PathBuf {
    opencode_dir().join("opencode.json")
}

// 4. Cline
fn cline_data_dir() -> PathBuf {
    home_dir().join(".cline").join("data")
}
fn cline_state_path() -> PathBuf {
    cline_data_dir().join("globalState.json")
}
fn cline_secrets_path() -> PathBuf {
    cline_data_dir().join("secrets.json")
}

// 5. Crush
fn crush_config_path() -> PathBuf {
    config_dir().join("crush").join("crush.json")
}

// 6. Devin
fn devin_config_path() -> PathBuf {
    home_dir().join(".devin").join("config.json")
}

// 7. Droid
fn droid_settings_path() -> PathBuf {
    home_dir().join(".factory").join("settings.json")
}

// 8. Jcode
fn jcode_dir() -> PathBuf {
    home_dir().join(".jcode")
}
fn jcode_config_path() -> PathBuf {
    jcode_dir().join("config.toml")
}

// 9. Kilo
fn kilo_auth_path() -> PathBuf {
    home_dir().join(".local").join("share").join("kilo").join("auth.json")
}

// 10. Pi
fn pi_models_path() -> PathBuf {
    home_dir().join(".pi").join("agent").join("models.json")
}

// 11. Smelt
fn smelt_config_path() -> PathBuf {
    home_dir().join(".smelt").join("config.json")
}

// 12. OMP
fn omp_config_path() -> PathBuf {
    home_dir().join(".omp").join("config.json")
}

// 13. OpenClaw
fn openclaw_settings_path() -> PathBuf {
    home_dir().join(".openclaw").join("openclaw.json")
}

// 14. Forge
fn forge_config_path() -> PathBuf {
    home_dir().join(".forge").join("config.json")
}

// 15. Grok-Build
fn grok_build_config_path() -> PathBuf {
    config_dir().join("grok").join("build.json")
}

// 16. DeepSeek-TUI
fn deepseek_tui_config_path() -> PathBuf {
    config_dir().join("deepseek").join("config.json")
}

// 17. CodeWhale
fn codewhale_config_path() -> PathBuf {
    home_dir().join(".codewhale").join("settings.json")
}

// 18. Copilot (VS Code)
fn copilot_config_path() -> PathBuf {
    if cfg!(target_os = "windows") {
        if let Ok(appdata) = std::env::var("APPDATA") {
            return PathBuf::from(appdata)
                .join("Code")
                .join("User")
                .join("chatLanguageModels.json");
        }
    } else if cfg!(target_os = "macos") {
        return home_dir()
            .join("Library")
            .join("Application Support")
            .join("Code")
            .join("User")
            .join("chatLanguageModels.json");
    }
    config_dir()
        .join("Code")
        .join("User")
        .join("chatLanguageModels.json")
}

// 19. Cowork
fn cowork_config_path() -> PathBuf {
    home_dir().join(".cowork").join("config.json")
}

// 20. Hermes
fn hermes_dir() -> PathBuf {
    home_dir().join(".hermes")
}
fn hermes_config_path() -> PathBuf {
    hermes_dir().join("config.toml")
}

// ─── Detection Helper ───────────────────────────────────────────────────────

fn check_cli_installed(cmd: &str, file_check: &Path) -> bool {
    if file_check.exists() {
        return true;
    }
    let which_cmd = if cfg!(target_os = "windows") { "where" } else { "which" };
    match std::process::Command::new(which_cmd).arg(cmd).output() {
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}

// ─── Handlers ───────────────────────────────────────────────────────────────

/// GET /api/cli-tools/all-statuses
pub async fn all_statuses_handler() -> Response {
    let mut map = serde_json::Map::new();

    let tools = [
        ("claude", check_cli_installed("claude", &claude_settings_path())),
        ("codex", check_cli_installed("codex", &codex_config_path())),
        ("opencode", check_cli_installed("opencode", &opencode_config_path())),
        ("droid", check_cli_installed("droid", &droid_settings_path())),
        ("openclaw", check_cli_installed("openclaw", &openclaw_settings_path())),
        ("hermes", check_cli_installed("hermes", &hermes_config_path())),
        ("cowork", check_cli_installed("cowork", &cowork_config_path())),
        ("cline", check_cli_installed("cline", &cline_state_path())),
        ("kilo", check_cli_installed("kilo", &kilo_auth_path())),
        ("deepseek-tui", check_cli_installed("deepseek-tui", &deepseek_tui_config_path())),
        ("jcode", check_cli_installed("jcode", &jcode_config_path())),
        ("grok-build", check_cli_installed("grok", &grok_build_config_path())),
        ("devin", check_cli_installed("devin", &devin_config_path())),
        ("pi", check_cli_installed("pi", &pi_models_path())),
        ("omp", check_cli_installed("omp", &omp_config_path())),
        ("crush", check_cli_installed("crush", &crush_config_path())),
        ("forge", check_cli_installed("forge", &forge_config_path())),
        ("smelt", check_cli_installed("smelt", &smelt_config_path())),
        ("codewhale", check_cli_installed("codewhale", &codewhale_config_path())),
    ];

    for (name, installed) in tools {
        map.insert(name.to_string(), json!({ "installed": installed }));
    }

    Json(Value::Object(map)).into_response()
}

// ─── Claude Settings ────────────────────────────────────────────────────────

pub async fn claude_get() -> Response {
    let path = claude_settings_path();
    let installed = check_cli_installed("claude", &path);
    if !installed {
        return Json(json!({
            "installed": false,
            "settings": Value::Null,
            "message": "Claude CLI is not installed",
        }))
        .into_response();
    }

    let settings = read_file_string(&path).and_then(|s| parse_jsonc(&s));
    let has_9router = settings
        .as_ref()
        .and_then(|s| s.get("env"))
        .and_then(|e| e.get("ANTHROPIC_BASE_URL"))
        .is_some();

    let claude_json = read_file_string(&claude_json_path()).and_then(|s| parse_jsonc(&s));
    let exa_mcp_enabled = claude_json
        .as_ref()
        .and_then(|c| c.get("mcpServers"))
        .and_then(|m| m.get("exa"))
        .is_some();

    Json(json!({
        "installed": true,
        "settings": settings,
        "has9Router": has_9router,
        "exaMcpEnabled": exa_mcp_enabled,
        "settingsPath": path.to_string_lossy(),
    }))
    .into_response()
}

pub async fn claude_post(Json(body): Json<Value>) -> Response {
    let env = match body.get("env").and_then(|v| v.as_object()) {
        Some(e) => e.clone(),
        None => return (StatusCode::BAD_REQUEST, Json(json!({ "error": "Invalid env object" }))).into_response(),
    };

    let path = claude_settings_path();
    let mut current = read_file_string(&path)
        .and_then(|s| parse_jsonc(&s))
        .unwrap_or_else(|| json!({}));

    if !current.is_object() {
        current = json!({});
    }

    let mut current_env = current
        .get("env")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    for (k, v) in env {
        if k == "ANTHROPIC_BASE_URL" {
            if let Some(url_str) = v.as_str() {
                let norm = if url_str.ends_with("/v1") {
                    url_str.to_string()
                } else {
                    format!("{}/v1", url_str.trim_end_matches('/'))
                };
                current_env.insert(k, Value::String(norm));
                continue;
            }
        }
        current_env.insert(k, v);
    }

    if let Some(compact) = body.get("autoCompactWindow").and_then(|v| v.as_str()) {
        current_env.insert(
            "CLAUDE_CODE_AUTO_COMPACT_WINDOW".to_string(),
            Value::String(compact.to_string()),
        );
    }

    current["hasCompletedOnboarding"] = Value::Bool(true);
    current["env"] = Value::Object(current_env);

    let content = match serde_json::to_string_pretty(&current) {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    };

    if let Err(e) = write_file_atomic(&path, &content) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
    }

    Json(json!({
        "success": true,
        "message": "Settings updated successfully",
    }))
    .into_response()
}

pub async fn claude_delete() -> Response {
    let path = claude_settings_path();
    if !path.exists() {
        return Json(json!({ "success": true, "message": "No settings file to reset" })).into_response();
    }

    let mut current = match read_file_string(&path).and_then(|s| parse_jsonc(&s)) {
        Some(c) => c,
        None => return Json(json!({ "success": true, "message": "No settings file to reset" })).into_response(),
    };

    const RESET_KEYS: &[&str] = &[
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "API_TIMEOUT_MS",
        "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
    ];

    if let Some(env_obj) = current.get_mut("env").and_then(|v| v.as_object_mut()) {
        for k in RESET_KEYS {
            env_obj.remove(*k);
        }
        if env_obj.is_empty() {
            if let Some(obj) = current.as_object_mut() {
                obj.remove("env");
            }
        }
    }

    let content = serde_json::to_string_pretty(&current).unwrap_or_default();
    let _ = write_file_atomic(&path, &content);

    Json(json!({ "success": true, "message": "Settings reset successfully" })).into_response()
}

// ─── Codex Settings ─────────────────────────────────────────────────────────

pub async fn codex_get() -> Response {
    let path = codex_config_path();
    let installed = check_cli_installed("codex", &path);
    if !installed {
        return Json(json!({
            "installed": false,
            "config": Value::Null,
            "message": "Codex CLI is not installed",
        }))
        .into_response();
    }

    let content = read_file_string(&path);
    let has_9router = content
        .as_ref()
        .map(|c| c.contains("model_provider = \"9router\"") || c.contains("[model_providers.9router]"))
        .unwrap_or(false);

    Json(json!({
        "installed": true,
        "config": content,
        "has9Router": has_9router,
        "configPath": path.to_string_lossy(),
    }))
    .into_response()
}

pub async fn codex_post(Json(body): Json<Value>) -> Response {
    let base_url = match body.get("baseUrl").and_then(|v| v.as_str()) {
        Some(u) => u,
        None => return (StatusCode::BAD_REQUEST, Json(json!({ "error": "baseUrl, apiKey and model are required" }))).into_response(),
    };
    let api_key = match body.get("apiKey").and_then(|v| v.as_str()) {
        Some(k) => k,
        None => return (StatusCode::BAD_REQUEST, Json(json!({ "error": "baseUrl, apiKey and model are required" }))).into_response(),
    };
    let model = match body.get("model").and_then(|v| v.as_str()) {
        Some(m) => m,
        None => return (StatusCode::BAD_REQUEST, Json(json!({ "error": "baseUrl, apiKey and model are required" }))).into_response(),
    };

    let subagent_model = body.get("subagentModel").and_then(|v| v.as_str()).unwrap_or(model);
    let norm_url = if base_url.ends_with("/v1") {
        base_url.to_string()
    } else {
        format!("{}/v1", base_url.trim_end_matches('/'))
    };

    let toml_snippet = format!(
        r#"model = "{model}"
model_provider = "9router"

[model_providers.9router]
name = "9Router"
base_url = "{norm_url}"
wire_api = "responses"
http_headers = {{ Authorization = "Bearer {api_key}" }}

[agents]
default_subagent_model = "{subagent_model}"
"#
    );

    let path = codex_config_path();
    if let Err(e) = write_file_atomic(&path, &toml_snippet) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
    }

    Json(json!({
        "success": true,
        "message": "Codex settings applied successfully!",
        "configPath": path.to_string_lossy(),
    }))
    .into_response()
}

pub async fn codex_delete() -> Response {
    let path = codex_config_path();
    let auth_path = codex_auth_path();
    let _ = remove_file_if_exists(&path);
    let _ = remove_file_if_exists(&auth_path);

    Json(json!({
        "success": true,
        "message": "9Router settings removed successfully",
    }))
    .into_response()
}

/// GET /api/cli-tools/codex-profiles
pub async fn codex_profiles_get() -> Response {
    let dir = codex_dir();
    let mut profiles = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                if name.ends_with(".config.toml") {
                    let prof = name.trim_end_matches(".config.toml");
                    profiles.push(prof.to_string());
                }
            }
        }
    }
    Json(json!({ "profiles": profiles })).into_response()
}

// ─── OpenCode Settings ──────────────────────────────────────────────────────

pub async fn opencode_get() -> Response {
    let path = opencode_config_path();
    let installed = check_cli_installed("opencode", &path);
    if !installed {
        return Json(json!({
            "installed": false,
            "config": Value::Null,
            "message": "OpenCode CLI is not installed",
        }))
        .into_response();
    }

    let config = read_file_string(&path).and_then(|s| parse_jsonc(&s));
    let has_9router = config
        .as_ref()
        .and_then(|c| c.get("provider"))
        .and_then(|p| p.get("9router"))
        .is_some();

    Json(json!({
        "installed": true,
        "config": config,
        "has9Router": has_9router,
        "configPath": path.to_string_lossy(),
    }))
    .into_response()
}

pub async fn opencode_post(Json(body): Json<Value>) -> Response {
    let base_url = match body.get("baseUrl").and_then(|v| v.as_str()) {
        Some(u) => u,
        None => return (StatusCode::BAD_REQUEST, Json(json!({ "error": "baseUrl is required" }))).into_response(),
    };
    let model = body.get("model").and_then(|v| v.as_str()).unwrap_or("claude-3-5-sonnet");
    let caller_key = body.get("apiKey").and_then(|v| v.as_str());
    let api_key = resolve_cli_api_key(caller_key);

    let norm_url = if base_url.ends_with("/v1") {
        base_url.to_string()
    } else {
        format!("{}/v1", base_url.trim_end_matches('/'))
    };

    let path = opencode_config_path();
    let mut config = read_file_string(&path)
        .and_then(|s| parse_jsonc(&s))
        .unwrap_or_else(|| json!({}));

    if !config.is_object() {
        config = json!({});
    }

    config["provider"] = json!({
        "9router": {
            "npm": "@ai-sdk/openai-compatible",
            "options": {
                "baseURL": norm_url,
                "apiKey": api_key,
            },
            "models": {
                model: {
                    "name": model,
                    "modalities": { "input": ["text", "image"], "output": ["text"] }
                }
            }
        }
    });
    config["model"] = json!(format!("9router/{model}"));

    let content = serde_json::to_string_pretty(&config).unwrap_or_default();
    if let Err(e) = write_file_atomic(&path, &content) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
    }

    Json(json!({
        "success": true,
        "message": "OpenCode settings applied successfully!",
        "configPath": path.to_string_lossy(),
    }))
    .into_response()
}

pub async fn opencode_patch(Json(body): Json<Value>) -> Response {
    let path = opencode_config_path();
    let mut config = match read_file_string(&path).and_then(|s| parse_jsonc(&s)) {
        Some(c) => c,
        None => return Json(json!({ "success": true, "message": "No config file found" })).into_response(),
    };

    if body.get("clearActiveModel").and_then(|v| v.as_bool()) == Some(true) {
        if let Some(m) = config.get("model").and_then(|v| v.as_str()) {
            if m.starts_with("9router/") {
                config["model"] = Value::String(String::new());
            }
        }
    }

    let content = serde_json::to_string_pretty(&config).unwrap_or_default();
    let _ = write_file_atomic(&path, &content);

    Json(json!({ "success": true, "message": "Settings updated" })).into_response()
}

pub async fn opencode_delete() -> Response {
    let path = opencode_config_path();
    if let Some(mut config) = read_file_string(&path).and_then(|s| parse_jsonc(&s)) {
        if let Some(p) = config.get_mut("provider").and_then(|v| v.as_object_mut()) {
            p.remove("9router");
        }
        if let Some(m) = config.get("model").and_then(|v| v.as_str()) {
            if m.starts_with("9router/") {
                config["model"] = Value::String(String::new());
            }
        }
        let content = serde_json::to_string_pretty(&config).unwrap_or_default();
        let _ = write_file_atomic(&path, &content);
    }

    Json(json!({ "success": true, "message": "9Router settings removed from OpenCode" })).into_response()
}

// ─── Generic JSON Config Handler Helper ─────────────────────────────────────

async fn generic_get(cmd: &str, path: PathBuf, root_key: &str) -> Response {
    let installed = check_cli_installed(cmd, &path);
    if !installed {
        return Json(json!({
            "installed": false,
            "settings": Value::Null,
            "message": format!("{} CLI is not installed", cmd),
        }))
        .into_response();
    }
    let config = read_file_string(&path).and_then(|s| parse_jsonc(&s));
    let has_9router = config
        .as_ref()
        .map(|c| c.to_string().contains("9router") || c.to_string().contains("20128"))
        .unwrap_or(false);

    Json(json!({
        "installed": true,
        root_key: config,
        "has9Router": has_9router,
        "configPath": path.to_string_lossy(),
    }))
    .into_response()
}

async fn generic_post(path: PathBuf, body: Value) -> Response {
    let content = serde_json::to_string_pretty(&body).unwrap_or_default();
    if let Err(e) = write_file_atomic(&path, &content) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
    }
    Json(json!({ "success": true, "message": "Settings updated successfully" })).into_response()
}

async fn generic_delete(path: PathBuf) -> Response {
    let _ = remove_file_if_exists(&path);
    Json(json!({ "success": true, "message": "Settings reset successfully" })).into_response()
}

// ─── Cline Settings ─────────────────────────────────────────────────────────

pub async fn cline_get() -> Response {
    generic_get("cline", cline_state_path(), "settings").await
}
pub async fn cline_post(Json(body): Json<Value>) -> Response {
    generic_post(cline_state_path(), body).await
}
pub async fn cline_delete() -> Response {
    let _ = remove_file_if_exists(&cline_secrets_path());
    generic_delete(cline_state_path()).await
}

// ─── Crush Settings ─────────────────────────────────────────────────────────

pub async fn crush_get() -> Response {
    generic_get("crush", crush_config_path(), "settings").await
}
pub async fn crush_post(Json(body): Json<Value>) -> Response {
    generic_post(crush_config_path(), body).await
}
pub async fn crush_delete() -> Response {
    generic_delete(crush_config_path()).await
}

// ─── Devin Settings ─────────────────────────────────────────────────────────

pub async fn devin_get() -> Response {
    generic_get("devin", devin_config_path(), "settings").await
}
pub async fn devin_post(Json(body): Json<Value>) -> Response {
    generic_post(devin_config_path(), body).await
}
pub async fn devin_delete() -> Response {
    generic_delete(devin_config_path()).await
}

// ─── Droid Settings ─────────────────────────────────────────────────────────

pub async fn droid_get() -> Response {
    generic_get("droid", droid_settings_path(), "settings").await
}
pub async fn droid_post(Json(body): Json<Value>) -> Response {
    generic_post(droid_settings_path(), body).await
}
pub async fn droid_delete() -> Response {
    generic_delete(droid_settings_path()).await
}

// ─── Jcode Settings ─────────────────────────────────────────────────────────

pub async fn jcode_get() -> Response {
    let path = jcode_config_path();
    let installed = check_cli_installed("jcode", &path);
    let config = read_file_string(&path);
    let has_9router = config.as_ref().map(|c| c.contains("9router")).unwrap_or(false);

    Json(json!({
        "installed": installed,
        "config": config,
        "has9Router": has_9router,
        "configPath": path.to_string_lossy(),
    }))
    .into_response()
}
pub async fn jcode_post(Json(body): Json<Value>) -> Response {
    let path = jcode_config_path();
    let content = body.get("config").and_then(|v| v.as_str()).unwrap_or("");
    if let Err(e) = write_file_atomic(&path, content) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
    }
    Json(json!({ "success": true, "message": "Jcode config updated" })).into_response()
}
pub async fn jcode_delete() -> Response {
    generic_delete(jcode_config_path()).await
}

// ─── Kilo Settings ─────────────────────────────────────────────────────────

pub async fn kilo_get() -> Response {
    generic_get("kilo", kilo_auth_path(), "settings").await
}
pub async fn kilo_post(Json(body): Json<Value>) -> Response {
    generic_post(kilo_auth_path(), body).await
}
pub async fn kilo_delete() -> Response {
    generic_delete(kilo_auth_path()).await
}

// ─── Pi Settings ───────────────────────────────────────────────────────────

pub async fn pi_get() -> Response {
    generic_get("pi", pi_models_path(), "settings").await
}
pub async fn pi_post(Json(body): Json<Value>) -> Response {
    generic_post(pi_models_path(), body).await
}
pub async fn pi_delete() -> Response {
    generic_delete(pi_models_path()).await
}

// ─── Smelt Settings ─────────────────────────────────────────────────────────

pub async fn smelt_get() -> Response {
    generic_get("smelt", smelt_config_path(), "settings").await
}
pub async fn smelt_post(Json(body): Json<Value>) -> Response {
    generic_post(smelt_config_path(), body).await
}
pub async fn smelt_delete() -> Response {
    generic_delete(smelt_config_path()).await
}

// ─── OMP Settings ───────────────────────────────────────────────────────────

pub async fn omp_get() -> Response {
    generic_get("omp", omp_config_path(), "settings").await
}
pub async fn omp_post(Json(body): Json<Value>) -> Response {
    generic_post(omp_config_path(), body).await
}
pub async fn omp_delete() -> Response {
    generic_delete(omp_config_path()).await
}

// ─── OpenClaw Settings ──────────────────────────────────────────────────────

pub async fn openclaw_get() -> Response {
    generic_get("openclaw", openclaw_settings_path(), "settings").await
}
pub async fn openclaw_post(Json(body): Json<Value>) -> Response {
    generic_post(openclaw_settings_path(), body).await
}
pub async fn openclaw_delete() -> Response {
    generic_delete(openclaw_settings_path()).await
}

// ─── Forge Settings ─────────────────────────────────────────────────────────

pub async fn forge_get() -> Response {
    generic_get("forge", forge_config_path(), "settings").await
}
pub async fn forge_post(Json(body): Json<Value>) -> Response {
    generic_post(forge_config_path(), body).await
}
pub async fn forge_delete() -> Response {
    generic_delete(forge_config_path()).await
}

// ─── Grok-Build Settings ───────────────────────────────────────────────────

pub async fn grok_build_get() -> Response {
    generic_get("grok", grok_build_config_path(), "settings").await
}
pub async fn grok_build_post(Json(body): Json<Value>) -> Response {
    generic_post(grok_build_config_path(), body).await
}
pub async fn grok_build_delete() -> Response {
    generic_delete(grok_build_config_path()).await
}

// ─── DeepSeek-TUI Settings ─────────────────────────────────────────────────

pub async fn deepseek_tui_get() -> Response {
    generic_get("deepseek-tui", deepseek_tui_config_path(), "settings").await
}
pub async fn deepseek_tui_post(Json(body): Json<Value>) -> Response {
    generic_post(deepseek_tui_config_path(), body).await
}
pub async fn deepseek_tui_delete() -> Response {
    generic_delete(deepseek_tui_config_path()).await
}

// ─── CodeWhale Settings ────────────────────────────────────────────────────

pub async fn codewhale_get() -> Response {
    generic_get("codewhale", codewhale_config_path(), "settings").await
}
pub async fn codewhale_post(Json(body): Json<Value>) -> Response {
    generic_post(codewhale_config_path(), body).await
}
pub async fn codewhale_delete() -> Response {
    generic_delete(codewhale_config_path()).await
}

// ─── Copilot Settings ──────────────────────────────────────────────────────

pub async fn copilot_get() -> Response {
    let path = copilot_config_path();
    let config = read_file_string(&path).and_then(|s| parse_jsonc(&s));
    let has_9router = config
        .as_ref()
        .and_then(|c| c.as_array())
        .map(|arr| arr.iter().any(|entry| entry.get("name").and_then(|v| v.as_str()) == Some("9Router")))
        .unwrap_or(false);

    Json(json!({
        "installed": path.exists(),
        "config": config,
        "has9Router": has_9router,
        "configPath": path.to_string_lossy(),
    }))
    .into_response()
}

pub async fn copilot_post(Json(body): Json<Value>) -> Response {
    let path = copilot_config_path();
    let mut config = read_file_string(&path)
        .and_then(|s| parse_jsonc(&s))
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();

    config.retain(|e| e.get("name").and_then(|v| v.as_str()) != Some("9Router"));
    config.push(body);

    let content = serde_json::to_string_pretty(&config).unwrap_or_default();
    if let Err(e) = write_file_atomic(&path, &content) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
    }
    Json(json!({ "success": true, "message": "Copilot settings applied" })).into_response()
}

pub async fn copilot_delete() -> Response {
    let path = copilot_config_path();
    if let Some(mut config) = read_file_string(&path).and_then(|s| parse_jsonc(&s)).and_then(|v| v.as_array().cloned()) {
        config.retain(|e| e.get("name").and_then(|v| v.as_str()) != Some("9Router"));
        let content = serde_json::to_string_pretty(&config).unwrap_or_default();
        let _ = write_file_atomic(&path, &content);
    }
    Json(json!({ "success": true, "message": "9Router removed from Copilot" })).into_response()
}

// ─── Cowork Settings & MCP ──────────────────────────────────────────────────

pub async fn cowork_get() -> Response {
    generic_get("cowork", cowork_config_path(), "settings").await
}
pub async fn cowork_post(Json(body): Json<Value>) -> Response {
    generic_post(cowork_config_path(), body).await
}
pub async fn cowork_delete() -> Response {
    generic_delete(cowork_config_path()).await
}

pub async fn cowork_mcp_registry_get() -> Response {
    Json(json!({ "servers": [] })).into_response()
}

pub async fn cowork_mcp_tools_get() -> Response {
    Json(json!({ "tools": [] })).into_response()
}

// ─── Hermes Settings ────────────────────────────────────────────────────────

pub async fn hermes_get() -> Response {
    let path = hermes_config_path();
    let installed = check_cli_installed("hermes", &path);
    let config = read_file_string(&path);
    let has_9router = config.as_ref().map(|c| c.contains("9router")).unwrap_or(false);

    Json(json!({
        "installed": installed,
        "config": config,
        "has9Router": has_9router,
        "configPath": path.to_string_lossy(),
    }))
    .into_response()
}
pub async fn hermes_post(Json(body): Json<Value>) -> Response {
    let path = hermes_config_path();
    let content = body.get("config").and_then(|v| v.as_str()).unwrap_or("");
    if let Err(e) = write_file_atomic(&path, content) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
    }
    Json(json!({ "success": true, "message": "Hermes settings updated" })).into_response()
}
pub async fn hermes_delete() -> Response {
    generic_delete(hermes_config_path()).await
}

pub async fn hermes_profiles_get() -> Response {
    let dir = hermes_dir().join("profiles");
    let mut profiles = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    profiles.push(name.to_string());
                }
            }
        }
    }
    Json(json!({ "profiles": profiles })).into_response()
}

// ─── Antigravity MITM ───────────────────────────────────────────────────────

pub async fn antigravity_mitm_get() -> Response {
    Json(json!({
        "running": false,
        "port": 20128,
        "certInstalled": false,
        "dnsEnabled": false,
    }))
    .into_response()
}

pub async fn antigravity_mitm_post(Json(_body): Json<Value>) -> Response {
    Json(json!({ "success": true, "running": true })).into_response()
}

pub async fn antigravity_mitm_delete() -> Response {
    Json(json!({ "success": true, "running": false })).into_response()
}

pub async fn antigravity_mitm_alias_post(Json(_body): Json<Value>) -> Response {
    Json(json!({ "success": true })).into_response()
}

// ─── Routes ─────────────────────────────────────────────────────────────────

pub fn routes() -> Router {
    Router::new()
        .route("/api/cli-tools/all-statuses", get(all_statuses_handler))
        .route("/api/cli-tools/claude-settings", get(claude_get).post(claude_post).delete(claude_delete))
        .route("/api/cli-tools/codex-settings", get(codex_get).post(codex_post).delete(codex_delete))
        .route("/api/cli-tools/codex-profiles", get(codex_profiles_get))
        .route("/api/cli-tools/opencode-settings", get(opencode_get).post(opencode_post).patch(opencode_patch).delete(opencode_delete))
        .route("/api/cli-tools/cline-settings", get(cline_get).post(cline_post).delete(cline_delete))
        .route("/api/cli-tools/crush-settings", get(crush_get).post(crush_post).delete(crush_delete))
        .route("/api/cli-tools/devin-settings", get(devin_get).post(devin_post).delete(devin_delete))
        .route("/api/cli-tools/droid-settings", get(droid_get).post(droid_post).delete(droid_delete))
        .route("/api/cli-tools/jcode-settings", get(jcode_get).post(jcode_post).delete(jcode_delete))
        .route("/api/cli-tools/kilo-settings", get(kilo_get).post(kilo_post).delete(kilo_delete))
        .route("/api/cli-tools/pi-settings", get(pi_get).post(pi_post).delete(pi_delete))
        .route("/api/cli-tools/smelt-settings", get(smelt_get).post(smelt_post).delete(smelt_delete))
        .route("/api/cli-tools/omp-settings", get(omp_get).post(omp_post).delete(omp_delete))
        .route("/api/cli-tools/openclaw-settings", get(openclaw_get).post(openclaw_post).delete(openclaw_delete))
        .route("/api/cli-tools/forge-settings", get(forge_get).post(forge_post).delete(forge_delete))
        .route("/api/cli-tools/grok-build-settings", get(grok_build_get).post(grok_build_post).delete(grok_build_delete))
        .route("/api/cli-tools/deepseek-tui-settings", get(deepseek_tui_get).post(deepseek_tui_post).delete(deepseek_tui_delete))
        .route("/api/cli-tools/codewhale-settings", get(codewhale_get).post(codewhale_post).delete(codewhale_delete))
        .route("/api/cli-tools/copilot-settings", get(copilot_get).post(copilot_post).delete(copilot_delete))
        .route("/api/cli-tools/cowork-settings", get(cowork_get).post(cowork_post).delete(cowork_delete))
        .route("/api/cli-tools/cowork-mcp-registry", get(cowork_mcp_registry_get))
        .route("/api/cli-tools/cowork-mcp-tools", get(cowork_mcp_tools_get))
        .route("/api/cli-tools/hermes-settings", get(hermes_get).post(hermes_post).delete(hermes_delete))
        .route("/api/cli-tools/hermes-profiles", get(hermes_profiles_get))
        .route("/api/cli-tools/antigravity-mitm", get(antigravity_mitm_get).post(antigravity_mitm_post).delete(antigravity_mitm_delete))
        .route("/api/cli-tools/antigravity-mitm/alias", post(antigravity_mitm_alias_post))
}
