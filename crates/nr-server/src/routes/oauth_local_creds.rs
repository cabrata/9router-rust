//! Local credential discovery for the `oauth/*/auto-import` routes.
//!
//! Port of:
//! - `src/lib/oauth/utils/perchCliCredentials.js`
//! - `src/lib/oauth/utils/zedCredentials.js` (`readZedIdeCredentials`)
//! - the filesystem half of each `src/app/api/oauth/*/auto-import/route.js`
//!
//! Only reads well-known IDE/CLI state; never writes outside `$DATA_DIR`.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

fn home() -> PathBuf {
    nr_core::paths::home_dir()
}

/// Run a command with a bounded wait, returning trimmed stdout on success.
fn run(program: &str, args: &[&str], timeout: std::time::Duration) -> Option<String> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd.spawn().ok()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                use std::io::Read;
                let mut buf = String::new();
                child.stdout.take()?.read_to_string(&mut buf).ok()?;
                return Some(buf.trim().to_string());
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(_) => return None,
        }
    }
}

fn read_json(path: &Path) -> Option<Value> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// `normalize(value)`: a JSON-stringified string is unwrapped, anything else is
/// returned as-is. Used for values stored as `"\"abc\""` in the Cursor DB.
pub fn normalize_sqlite_value(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2 {
        if let Ok(Value::String(inner)) = serde_json::from_str::<Value>(trimmed) {
            return inner;
        }
    }
    trimmed.to_string()
}

// ─── Command Code ───────────────────────────────────────────────────────────

/// `getCandidatePaths()` of `commandcode/auto-import`.
pub fn commandcode_paths() -> Vec<PathBuf> {
    let mut paths = vec![home().join(".commandcode").join("auth.json")];
    if cfg!(windows) {
        let appdata = std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home().join("AppData").join("Roaming"));
        paths.push(appdata.join("commandcode").join("auth.json"));
    }
    paths
}

/// GET `/api/oauth/commandcode/auto-import`.
pub fn read_commandcode_credentials() -> Value {
    let candidates = commandcode_paths();
    let Some(auth_path) = candidates.iter().find(|p| p.is_file()) else {
        let checked = candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        return json!({
            "found": false,
            "error": format!(
                "Command Code auth file not found. Checked:\n{checked}\n\nRun 'commandcode login' in your terminal first."
            ),
        });
    };

    let Some(auth) = read_json(auth_path) else {
        return json!({
            "found": false,
            "error": "auth.json is not valid JSON. Please run 'commandcode login' again.",
        });
    };

    let api_key = auth
        .get("apiKey")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(api_key) = api_key else {
        return json!({
            "found": false,
            "error": "No apiKey found in ~/.commandcode/auth.json. Please run 'commandcode login'.",
        });
    };

    let pick = |key: &str| auth.get(key).cloned().unwrap_or(Value::Null);
    json!({
        "found": true,
        "apiKey": api_key,
        "userId": pick("userId"),
        "userName": pick("userName"),
        "keyName": pick("keyName"),
        "authenticatedAt": pick("authenticatedAt"),
        "source": auth_path.display().to_string(),
    })
}

// ─── Xiaomi MiMo ────────────────────────────────────────────────────────────

/// `getCandidatePaths()` of `xiaomi-mimo/auto-import`.
pub fn xiaomi_mimo_paths() -> Vec<PathBuf> {
    let home = home();
    let mut paths = vec![home.join(".local").join("share").join("mimocode").join("auth.json")];
    if cfg!(windows) {
        let appdata = std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join("AppData").join("Roaming"));
        paths.push(appdata.join("Xiaomi MiMo").join("auth.json"));
    }
    if cfg!(target_os = "macos") {
        paths.push(
            home.join("Library")
                .join("Application Support")
                .join("mimocode")
                .join("auth.json"),
        );
    }
    paths
}

/// GET `/api/oauth/xiaomi-mimo/auto-import`.
pub fn read_xiaomi_mimo_credentials() -> Value {
    let candidates = xiaomi_mimo_paths();
    let Some(auth_path) = candidates.iter().find(|p| p.is_file()) else {
        let checked = candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        return json!({
            "found": false,
            "error": format!(
                "Xiaomi MiMo Desktop auth file not found. Checked:\n{checked}\n\nMake sure Xiaomi MiMo Desktop is installed and you are signed in."
            ),
        });
    };

    let Some(auth) = read_json(auth_path) else {
        return json!({
            "found": false,
            "error": "auth.json is not valid JSON. Please sign in to Xiaomi MiMo Desktop again.",
        });
    };

    let xiaomi = auth.get("xiaomi").cloned().unwrap_or(Value::Null);
    let Some(key) = xiaomi
        .get("key")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return json!({
            "found": false,
            "error": "No Xiaomi credentials found in auth.json. Please sign in to Xiaomi MiMo Desktop.",
        });
    };

    if !key.starts_with("sk-") {
        return json!({
            "found": false,
            "error": "Xiaomi key does not appear to be a valid API key (expected sk- prefix).",
        });
    }

    let metadata = xiaomi.get("metadata").cloned().unwrap_or_else(|| json!({}));
    json!({
        "found": true,
        "apiKey": key,
        "uid": metadata.get("uid").cloned().unwrap_or(Value::Null),
        "baseUrl": metadata
            .get("base_url")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("https://api.xiaomimimo.com/v1"),
        "source": auth_path.display().to_string(),
        // shortcut: the Desktop cookie-store passToken needs the OS keyring and a
        // locked SQLite DB; left null so multi-account rotation falls back to the
        // API key alone. Upgrade when Desktop exposes it without the lock.
        "mimoPassToken": Value::Null,
        "mimoUserId": Value::Null,
        "mimoCUserId": Value::Null,
    })
}

// ─── Kiro ───────────────────────────────────────────────────────────────────

const KIRO_REFRESH_PREFIX: &str = "aorAAAAAG";

/// GET `/api/oauth/kiro/auto-import`: scan the AWS SSO cache for a Kiro token.
pub fn read_kiro_credentials() -> Value {
    let cache = home().join(".aws").join("sso").join("cache");
    let Ok(entries) = std::fs::read_dir(&cache) else {
        return json!({
            "found": false,
            "error": "AWS SSO cache not found. Please login to Kiro IDE first.",
        });
    };

    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .collect();
    names.sort();

    let matches_token = |data: &Value| {
        data.get("refreshToken")
            .and_then(|v| v.as_str())
            .map(|t| t.starts_with(KIRO_REFRESH_PREFIX))
            .unwrap_or(false)
    };

    // Prefer kiro-auth-token.json, then scan the rest in sorted order.
    let mut ordered = Vec::new();
    if names.iter().any(|n| n == "kiro-auth-token.json") {
        ordered.push("kiro-auth-token.json".to_string());
    }
    ordered.extend(names.into_iter().filter(|n| n != "kiro-auth-token.json"));

    let mut found: Option<(String, Value)> = None;
    for name in ordered {
        if !name.ends_with(".json") {
            continue;
        }
        let Some(data) = read_json(&cache.join(&name)) else {
            continue;
        };
        if matches_token(&data) {
            found = Some((name, data));
            break;
        }
    }

    let Some((source, token_data)) = found else {
        return json!({
            "found": false,
            "error": "Kiro token not found in AWS SSO cache. Please login to Kiro IDE first.",
        });
    };

    // IDC tokens link a client registration file by `clientIdHash`.
    let mut client_id = Value::Null;
    let mut client_secret = Value::Null;
    if let Some(hash) = token_data
        .get("clientIdHash")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        if let Some(client) = read_json(&cache.join(format!("{hash}.json"))) {
            let has_both = client.get("clientId").and_then(|v| v.as_str()).is_some()
                && client.get("clientSecret").and_then(|v| v.as_str()).is_some();
            if has_both {
                client_id = client.get("clientId").cloned().unwrap_or(Value::Null);
                client_secret = client.get("clientSecret").cloned().unwrap_or(Value::Null);
            }
        }
    }

    // The runtime gateway requires us-east-1 in the ARN regardless of IDC region.
    let mut profile_arn: Option<String> = None;
    let profile_paths = [
        std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home().join("AppData").join("Roaming"))
            .join("Kiro")
            .join("User")
            .join("globalStorage")
            .join("kiro.kiroagent")
            .join("profile.json"),
        home()
            .join(".config")
            .join("Kiro")
            .join("User")
            .join("globalStorage")
            .join("kiro.kiroagent")
            .join("profile.json"),
    ];
    for path in profile_paths {
        if let Some(profile) = read_json(&path) {
            if let Some(arn) = profile.get("arn").and_then(|v| v.as_str()) {
                profile_arn = Some(normalize_kiro_profile_arn(arn));
                break;
            }
        }
    }

    json!({
        "found": true,
        "refreshToken": token_data.get("refreshToken").cloned().unwrap_or(Value::Null),
        "source": source,
        "clientId": client_id,
        "clientSecret": client_secret,
        "region": token_data.get("region").cloned().unwrap_or(Value::Null),
        "authMethod": token_data.get("authMethod").cloned().unwrap_or(Value::Null),
        "profileArn": profile_arn.map(Value::String).unwrap_or(Value::Null),
    })
}

/// `arn.replace(/arn:aws:codewhisperer:[^:]+:/, "arn:aws:codewhisperer:us-east-1:")`.
pub fn normalize_kiro_profile_arn(arn: &str) -> String {
    const PREFIX: &str = "arn:aws:codewhisperer:";
    let Some(rest) = arn.strip_prefix(PREFIX) else {
        return arn.to_string();
    };
    match rest.split_once(':') {
        Some((_region, tail)) => format!("{PREFIX}us-east-1:{tail}"),
        None => arn.to_string(),
    }
}

// ─── Cursor ─────────────────────────────────────────────────────────────────

const CURSOR_ACCESS_TOKEN_KEYS: &[&str] = &["cursorAuth/accessToken", "cursorAuth/token"];
const CURSOR_MACHINE_ID_KEYS: &[&str] = &[
    "storage.serviceMachineId",
    "storage.machineId",
    "telemetry.machineId",
];

/// `getCandidatePaths(platform)` of `cursor/auto-import`.
pub fn cursor_db_paths() -> Vec<PathBuf> {
    let home = home();
    if cfg!(target_os = "macos") {
        return vec![
            home.join("Library/Application Support/Cursor/User/globalStorage/state.vscdb"),
            home.join("Library/Application Support/Cursor - Insiders/User/globalStorage/state.vscdb"),
        ];
    }
    if cfg!(windows) {
        let appdata = std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join("AppData").join("Roaming"));
        let local = std::env::var("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join("AppData").join("Local"));
        return vec![
            appdata.join("Cursor/User/globalStorage/state.vscdb"),
            appdata.join("Cursor - Insiders/User/globalStorage/state.vscdb"),
            local.join("Cursor/User/globalStorage/state.vscdb"),
            local.join("Programs/Cursor/User/globalStorage/state.vscdb"),
        ];
    }
    vec![
        home.join(".config/Cursor/User/globalStorage/state.vscdb"),
        home.join(".config/cursor/User/globalStorage/state.vscdb"),
    ]
}

/// Read one `itemTable` value from the Cursor VS Code state DB.
///
/// `// shortcut: opens the DB read-only through rusqlite instead of Node's
/// better-sqlite3; the `sqlite3` CLI fallback is unnecessary here because
/// rusqlite is bundled.`
pub fn read_cursor_item(db_path: &Path, key: &str) -> Option<String> {
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let mut stmt = conn
        .prepare("SELECT value FROM itemTable WHERE key = ?1 LIMIT 1")
        .ok()?;
    let value: Option<String> = stmt.query_row([key], |row| row.get(0)).ok();
    value.map(|v| normalize_sqlite_value(&v)).filter(|v| !v.is_empty())
}

/// GET `/api/oauth/cursor/auto-import`.
pub fn read_cursor_credentials() -> Value {
    let candidates = cursor_db_paths();
    let Some(db_path) = candidates.iter().find(|p| p.is_file()) else {
        let checked = candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        return json!({
            "found": false,
            "error": format!(
                "Cursor database not found. Checked locations:\n{checked}\n\nMake sure Cursor IDE is installed and opened at least once."
            ),
        });
    };

    // On Linux, leftover config without an install is a false positive.
    if cfg!(target_os = "linux") {
        let installed = run("which", &["cursor"], std::time::Duration::from_secs(5)).is_some()
            || home()
                .join(".local/share/applications/cursor.desktop")
                .is_file();
        if !installed {
            return json!({
                "found": false,
                "error": "Cursor config files found but Cursor IDE does not appear to be installed. Skipping auto-import.",
            });
        }
    }

    let access_token = CURSOR_ACCESS_TOKEN_KEYS
        .iter()
        .find_map(|k| read_cursor_item(db_path, k));
    let machine_id = CURSOR_MACHINE_ID_KEYS
        .iter()
        .find_map(|k| read_cursor_item(db_path, k));

    match (access_token, machine_id) {
        (Some(access_token), Some(machine_id)) => json!({
            "found": true,
            "accessToken": access_token,
            "machineId": machine_id,
        }),
        _ => json!({
            "found": false,
            "windowsManual": true,
            "dbPath": db_path.display().to_string(),
        }),
    }
}

// ─── Perch ──────────────────────────────────────────────────────────────────

/// `parsePerchSession(raw)`: accept camelCase or snake_case session shapes.
pub fn parse_perch_session(raw: &str) -> Option<Value> {
    let data: Value = serde_json::from_str(raw).ok()?;
    let access_token = data
        .get("accessToken")
        .or_else(|| data.get("access_token"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;

    let user = data.get("user").cloned().unwrap_or(Value::Null);
    let refresh_token = data
        .get("refreshToken")
        .or_else(|| data.get("refresh_token"))
        .cloned()
        .filter(|v| !v.is_null());
    let user_id = data
        .get("userId")
        .or_else(|| data.get("user_id"))
        .or_else(|| user.get("id"))
        .cloned()
        .filter(|v| !v.is_null());
    let email = data
        .get("email")
        .or_else(|| user.get("email"))
        .cloned()
        .filter(|v| !v.is_null());

    Some(json!({
        "version": data.get("version").cloned().unwrap_or(json!(1)),
        "appUrl": data
            .get("appUrl")
            .and_then(|v| v.as_str())
            .unwrap_or("https://app.perchai.app"),
        "accessToken": access_token,
        "refreshToken": refresh_token.unwrap_or(Value::Null),
        "expiresAt": data
            .get("expiresAt")
            .filter(|v| v.is_number())
            .cloned()
            .unwrap_or(Value::Null),
        "userId": user_id.unwrap_or(Value::Null),
        "email": email.unwrap_or(Value::Null),
        "updatedAt": data.get("updatedAt").cloned().unwrap_or(Value::Null),
    }))
}

/// `readPerchCliCredentials()`: `~/.perch/cli-auth-session.json`, then the macOS
/// keychain when no explicit auth dir is set.
pub fn read_perch_cli_credentials() -> Value {
    let auth_dir = std::env::var("PERCH_CLI_AUTH_DIR")
        .ok()
        .map(|v| PathBuf::from(v.trim()))
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| home().join(".perch"));
    let auth_file = auth_dir.join("cli-auth-session.json");

    if let Ok(content) = std::fs::read_to_string(&auth_file) {
        if let Some(parsed) = parse_perch_session(&content) {
            let mut out = parsed;
            out["found"] = json!(true);
            out["source"] = json!("file");
            out["path"] = json!(auth_file.display().to_string());
            return out;
        }
    }

    let explicit_dir = std::env::var("PERCH_CLI_AUTH_DIR")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    if cfg!(target_os = "macos") && !explicit_dir {
        if let Some(stdout) = run(
            "security",
            &[
                "find-generic-password",
                "-s",
                "app.perchai.cli-auth",
                "-a",
                "default",
                "-w",
            ],
            std::time::Duration::from_secs(8),
        ) {
            if let Some(parsed) = parse_perch_session(&stdout) {
                let mut out = parsed;
                out["found"] = json!(true);
                out["source"] = json!("keychain");
                return out;
            }
        }
    }

    json!({
        "found": false,
        "error": "Perch CLI session not found. Please log in via `perch login`.",
    })
}

// ─── Zed ────────────────────────────────────────────────────────────────────

pub const ZED_DEFAULT_CREDENTIALS_URL: &str = "https://zed.dev";
pub const ZED_KEYRING_LABEL: &str = "zed-github-account";

fn zed_settings_path() -> PathBuf {
    let home = home();
    if cfg!(target_os = "macos") {
        return home.join("Library/Application Support/Zed/settings.json");
    }
    if cfg!(windows) {
        let local = std::env::var("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join("AppData").join("Local"));
        return local.join("Zed").join("settings.json");
    }
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".config"))
        .join("zed")
        .join("settings.json")
}

/// `resolveZedCredentialsUrl()`: `credentials_url` → `server_url` → default.
pub fn resolve_zed_credentials_url() -> String {
    if let Some(settings) = read_json(&zed_settings_path()) {
        for key in ["credentials_url", "server_url"] {
            if let Some(url) = settings.get(key).and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                return url.trim_end_matches('/').to_string();
            }
        }
    }
    ZED_DEFAULT_CREDENTIALS_URL.to_string()
}

/// Candidate `db.sqlite` paths holding Zed's `system_id`.
pub fn zed_global_db_paths() -> Vec<PathBuf> {
    let home = home();
    let channels = ["0-global", "0-stable", "0-preview"];
    let base = if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Zed/db")
    } else if cfg!(windows) {
        std::env::var("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join("AppData").join("Local"))
            .join("Zed")
            .join("db")
    } else {
        std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".local").join("share"))
            .join("zed")
            .join("db")
    };
    channels.iter().map(|c| base.join(c).join("db.sqlite")).collect()
}

/// `readZedSystemId()`: `system_id` from the kv_store, when present.
pub fn read_zed_system_id() -> Option<String> {
    for db_path in zed_global_db_paths() {
        if !db_path.is_file() {
            continue;
        }
        let Ok(conn) = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) else {
            continue;
        };
        let value: Option<String> = conn
            .prepare("SELECT value FROM kv_store WHERE key = 'system_id' LIMIT 1")
            .and_then(|mut stmt| stmt.query_row([], |row| row.get(0)))
            .ok();
        if let Some(value) = value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
            return Some(value);
        }
    }
    None
}

/// `parseSecretToolSearch`: pair `attribute.username` / `label` / `secret` blocks.
///
/// `secret-tool search --all url <url>` prints records separated by blank lines.
pub fn parse_secret_tool_search(output: &str) -> Vec<(String, String, String)> {
    let mut items: Vec<(String, String, String)> = Vec::new();
    let mut label = String::new();
    let mut username = String::new();
    let mut secret = String::new();

    let flush = |items: &mut Vec<(String, String, String)>,
                 label: &mut String,
                 username: &mut String,
                 secret: &mut String| {
        if !username.is_empty() || !secret.is_empty() {
            items.push((
                std::mem::take(label),
                std::mem::take(username),
                std::mem::take(secret),
            ));
        } else {
            label.clear();
            username.clear();
            secret.clear();
        }
    };

    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            flush(&mut items, &mut label, &mut username, &mut secret);
            continue;
        }
        if let Some(rest) = line.strip_prefix("label = ") {
            label = rest.to_string();
        } else if let Some(rest) = line.strip_prefix("secret = ") {
            secret = rest.to_string();
        } else if let Some(rest) = line.strip_prefix("attribute.username = ") {
            username = rest.to_string();
        }
    }
    flush(&mut items, &mut label, &mut username, &mut secret);
    items
}

/// `readZedIdeCredentials()`: session from the OS secret store + `system_id`.
pub fn read_zed_ide_credentials() -> Value {
    let credentials_url = resolve_zed_credentials_url();

    let pair = if cfg!(target_os = "linux") {
        read_zed_linux_credentials(&credentials_url)
    } else if cfg!(target_os = "macos") {
        read_zed_mac_credentials(&credentials_url)
    } else if cfg!(windows) {
        read_zed_windows_credentials(&credentials_url)
    } else {
        Err(format!(
            "Zed auto-import is not supported on platform {}",
            std::env::consts::OS
        ))
    };

    let (user_id, access_token) = match pair {
        Ok(pair) => pair,
        Err(message) => {
            return json!({
                "found": false,
                "error": message,
                "credentialsUrl": credentials_url,
            })
        }
    };

    let system_id = read_zed_system_id().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    json!({
        "found": true,
        "userId": user_id,
        "accessToken": access_token,
        "systemId": system_id,
        "credentialsUrl": credentials_url,
    })
}

fn read_zed_linux_credentials(credentials_url: &str) -> Result<(String, String), String> {
    let timeout = std::time::Duration::from_secs(8);
    if let Some(out) = run("secret-tool", &["search", "--all", "url", credentials_url], timeout) {
        let items = parse_secret_tool_search(&out);
        let best = items
            .iter()
            .find(|(label, user, secret)| {
                label == ZED_KEYRING_LABEL && !user.is_empty() && !secret.is_empty()
            })
            .or_else(|| items.iter().find(|(_, user, secret)| !user.is_empty() && !secret.is_empty()));
        if let Some((_, user, secret)) = best {
            return Ok((user.clone(), secret.clone()));
        }
    }

    match run("secret-tool", &["lookup", "url", credentials_url], timeout) {
        Some(secret) if !secret.is_empty() => Ok((credentials_url.to_string(), secret)),
        Some(_) => Err("Empty Zed keyring secret".into()),
        None => Err("Zed IDE session not found in the system keyring. Sign in to Zed, then retry.".into()),
    }
}

fn read_zed_mac_credentials(credentials_url: &str) -> Result<(String, String), String> {
    // `security find-internet-password -s <host> -g` prints the password to stderr.
    let host = url::Url::parse(credentials_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| "zed.dev".to_string());

    let mut cmd = std::process::Command::new("security");
    cmd.args(["find-internet-password", "-s", &host, "-g"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let Ok(out) = cmd.output() else {
        return Err("Zed IDE session not found in the keychain. Sign in to Zed, then retry.".into());
    };
    if !out.status.success() {
        return Err("Zed IDE session not found in the keychain. Sign in to Zed, then retry.".into());
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let password = stderr
        .lines()
        .find_map(|l| l.trim().strip_prefix("password: "))
        .map(|s| s.trim_matches('"').to_string())
        .filter(|s| !s.is_empty());
    // The account name is the first `"acct"<blob>=...` attribute.
    let account = stdout
        .lines()
        .find_map(|l| l.split("acct\"<blob>=").nth(1))
        .map(|s| s.trim_matches('"').to_string())
        .filter(|s| !s.is_empty());

    match (account, password) {
        (Some(account), Some(password)) => Ok((account, password)),
        _ => Err("Zed IDE session not found in the keychain. Sign in to Zed, then retry.".into()),
    }
}

fn read_zed_windows_credentials(credentials_url: &str) -> Result<(String, String), String> {
    // `cmdkey` cannot read secrets; the credential blob is written by Zed itself.
    //
    // shortcut: no Credential Manager read on Windows (needs wincred FFI). Point
    // the user at manual paste instead of pretending to auto-import.
    Err(format!(
        "Zed auto-import is not yet supported on Windows for {credentials_url}; paste the session manually."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_json_wrapped_sqlite_values() {
        assert_eq!(normalize_sqlite_value("\"tok\""), "tok");
        assert_eq!(normalize_sqlite_value("  plain  "), "plain");
        assert_eq!(normalize_sqlite_value("\"a\\\"b\""), "a\"b");
    }

    #[test]
    fn normalizes_kiro_profile_arn_region() {
        assert_eq!(
            normalize_kiro_profile_arn("arn:aws:codewhisperer:eu-west-1:123:profile/ABC"),
            "arn:aws:codewhisperer:us-east-1:123:profile/ABC"
        );
        assert_eq!(
            normalize_kiro_profile_arn("arn:aws:codewhisperer:us-east-1:123:profile/ABC"),
            "arn:aws:codewhisperer:us-east-1:123:profile/ABC"
        );
        // Non-codewhisperer ARNs are left alone.
        assert_eq!(normalize_kiro_profile_arn("arn:aws:iam::123:role/x"), "arn:aws:iam::123:role/x");
    }

    #[test]
    fn parses_perch_session_shapes() {
        let camel = parse_perch_session(
            r#"{"accessToken":"a","refreshToken":"r","userId":"u","email":"e@x","expiresAt":99}"#,
        )
        .expect("camel");
        assert_eq!(camel["accessToken"], json!("a"));
        assert_eq!(camel["refreshToken"], json!("r"));
        assert_eq!(camel["userId"], json!("u"));
        assert_eq!(camel["email"], json!("e@x"));
        assert_eq!(camel["expiresAt"], json!(99));

        let snake = parse_perch_session(
            r#"{"access_token":"a2","refresh_token":"r2","user":{"id":"u2","email":"e2"},"expiresAt":"x"}"#,
        )
        .expect("snake");
        assert_eq!(snake["accessToken"], json!("a2"));
        assert_eq!(snake["refreshToken"], json!("r2"));
        assert_eq!(snake["userId"], json!("u2"));
        assert_eq!(snake["email"], json!("e2"));
        assert_eq!(snake["expiresAt"], Value::Null, "only numeric expiresAt survives");

        assert!(parse_perch_session("{}").is_none());
        assert!(parse_perch_session("not json").is_none());
    }

    #[test]
    fn parses_secret_tool_records() {
        let output = "[/org/freedesktop/secrets/collection/login/1]\n\
lab=Zed\n\
label = zed-github-account\n\
secret = tok-1\n\
attribute.username = user-1\n\
\n\
[/org/freedesktop/secrets/collection/login/2]\n\
label = other\n\
secret = tok-2\n\
attribute.username = user-2\n";
        let items = parse_secret_tool_search(output);
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0],
            ("zed-github-account".into(), "user-1".into(), "tok-1".into())
        );
        assert_eq!(items[1].1, "user-2");
    }

    #[test]
    fn secret_tool_parser_ignores_partial_records() {
        let items = parse_secret_tool_search("label = lonely\n\n");
        assert!(items.is_empty());
    }

    #[test]
    fn commandcode_paths_are_home_relative() {
        let paths = commandcode_paths();
        assert!(paths[0].ends_with(".commandcode/auth.json"));
    }

    #[test]
    fn cursor_item_reads_absent_db_as_none() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(read_cursor_item(&dir.path().join("nope.vscdb"), "k").is_none());
    }

    #[test]
    fn cursor_item_reads_real_value() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("state.vscdb");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("CREATE TABLE itemTable (key TEXT, value TEXT)", []).unwrap();
        conn.execute(
            "INSERT INTO itemTable VALUES ('cursorAuth/accessToken', '\"tok-123\"')",
            [],
        )
        .unwrap();
        drop(conn);

        assert_eq!(
            read_cursor_item(&db, "cursorAuth/accessToken").as_deref(),
            Some("tok-123")
        );
        assert!(read_cursor_item(&db, "missing").is_none());
    }

    #[test]
    fn zed_system_id_reads_kv_store() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("db.sqlite");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("CREATE TABLE kv_store (key TEXT, value TEXT)", []).unwrap();
        conn.execute("INSERT INTO kv_store VALUES ('system_id', 'sys-9')", [])
            .unwrap();
        drop(conn);

        let conn = rusqlite::Connection::open_with_flags(
            &db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let value: String = conn
            .prepare("SELECT value FROM kv_store WHERE key = 'system_id' LIMIT 1")
            .unwrap()
            .query_row([], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "sys-9");
    }
}
