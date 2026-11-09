//! CLI command handlers for subcommands like `connect`, `show`, `xai video`.
//! These commands run without requiring the server to be started.
//! Port of cli/src/cli/commands/{connect,show,xaiVideo}.js.

use nr_cli::connect_tools::{self, ApplyCtx, ToolConfig};
use nr_cli::xai_video;
use crate::{CliCommand, XaiCommand};
use anyhow::{anyhow, Result};
use inquire::{Password, Select};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

const HELP_CONNECT: &str = r#"
Usage: 9router connect <server-url> [options]

Configure CLI tools on THIS machine to use a remote 9router server.
No local server is started.

  npx 9router connect http://<server-host>:20128
  npx 9router connect http://<server-host>:20128 --tools claude,codex,opencode

Options:
  --tools <list>         Comma-separated tools to configure (prompted if omitted
                         in a terminal; default: claude). Supported: see below, all
  --password <pw>        Dashboard password (or env NINE_ROUTER_PASSWORD)
  --save                 Save password for this server in ~/.9router/connect.env
  --key-name <name>      API key name to reuse/create (default: cli-<hostname>)
  --api-key <key>        Use this API key, skip login + key lookup
  --model <model>        Model for all non-Claude tools
  --print-env            Also print OpenAI-compatible env vars for other CLIs
  --reset                Remove 9router settings from the selected tools and exit
  -h, --help             Show this help

Supported tools: claude, codex, opencode, droid, crush, kilo, cline, pi, omp, all
"#;

const HELP_SHOW: &str = r#"
Usage: 9router show [tool…] [options]

Show the 9router settings each CLI tool on THIS machine is currently using.
Read-only: reads local config files, contacts no server.

  9router show                 # every supported tool
  9router show claude          # one tool
  9router show claude codex    # several (or: --tools claude,codex)

Options:
  --tools <list>   Comma-separated tools (same as positional names)
  --json           Machine-readable output (API key still masked)
  -h, --help       Show this help
"#;

/// Handle subcommands that don't require the server.
pub async fn handle_command(cmd: CliCommand) -> Result<()> {
    match cmd {
        CliCommand::Connect { url, extra } => handle_connect(&url, &extra).await,
        CliCommand::Show { tools } => handle_show(&tools),
        CliCommand::Update { extra } => handle_update(&extra).await,
        CliCommand::Xai { subcommand } => handle_xai(subcommand).await,
    }
}

#[derive(Debug, Default)]
struct ConnectOpts {
    url: Option<String>,
    tools: Option<Vec<String>>,
    password: Option<String>,
    key_name: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
    save: bool,
    print_env: bool,
    reset: bool,
    help: bool,
}

fn parse_connect_args(url: &str, extra: &[String]) -> Result<ConnectOpts> {
    let mut opts = ConnectOpts {
        url: Some(url.to_string()),
        key_name: Some(default_key_name()),
        password: std::env::var("NINE_ROUTER_PASSWORD").ok(),
        ..Default::default()
    };
    let mut i = 0;
    while i < extra.len() {
        let a = extra[i].as_str();
        let mut next = || -> Result<String> {
            i += 1;
            extra.get(i).cloned().ok_or_else(|| anyhow!("Missing value for {a}"))
        };
        match a {
            "--password" => opts.password = Some(next()?),
            "--key-name" => opts.key_name = Some(next()?),
            "--api-key" => opts.api_key = Some(next()?),
            "--tools" => {
                opts.tools = Some(next()?.split(',').map(String::from).collect())
            }
            "--model" => opts.model = Some(next()?),
            "--save" => opts.save = true,
            "--print-env" => opts.print_env = true,
            "--reset" => opts.reset = true,
            "-h" | "--help" => opts.help = true,
            _ => return Err(anyhow!("Unknown option: {a}")),
        }
        i += 1;
    }
    Ok(opts)
}

fn default_key_name() -> String {
    let host = hostname();
    format!("cli-{host}").chars().take(64).collect()
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".into())
}

async fn handle_connect(url: &str, extra: &[String]) -> Result<()> {
    let opts = parse_connect_args(url, extra)?;
    if opts.help {
        print!("{HELP_CONNECT}");
        return Ok(());
    }
    if !opts.reset && opts.url.as_deref().map(str::is_empty).unwrap_or(true) {
        print!("{HELP_CONNECT}");
        return Err(anyhow!("Server URL required"));
    }

    // Pick tools before any network call: bad names fail fast, and cancelling the
    // picker never leaves a freshly created key on the server.
    let tools = select_tools(&opts)?;

    if opts.reset {
        return run_reset(&tools);
    }

    let server = normalize_server_url(opts.url.as_deref().unwrap_or(""));
    println!("\n🔗 Configuring CLI tools for remote server");
    println!("   Server: {server}");

    let is_remote_http = server.starts_with("http://")
        && !server.starts_with("http://localhost:")
        && !server.starts_with("http://127.0.0.1:")
        && !server.starts_with("http://[::1]:");
    if is_remote_http {
        warn("Plain HTTP: password and API key travel unencrypted. Use only on a trusted LAN/VPN.");
    }

    // Resolve the API key: --api-key, else login with the dashboard password.
    let api_key: String;
    let mut cookie: Option<String> = None;
    match &opts.api_key {
        Some(key) => {
            println!("• Using provided API key");
            api_key = key.clone();
        }
        None => {
            // Password precedence: --password / env > saved (--save) file > prompt.
            let saved = if opts.password.is_none() { load_saved_password(&server) } else { None };
            if saved.is_some() {
                println!("• Using saved password ({})", saved_env_path().display());
            }
            let password = match opts.password.clone().or(saved) {
                Some(p) => p,
                None => prompt_password()?,
            };
            println!("• Logging in to {server}");
            let c = login(&server, &password).await?;
            cookie = Some(c.clone());
            if opts.save {
                save_password(&server, &password)?;
                println!("• Saved password to {} (plain text, mode 600)", saved_env_path().display());
            }
            let key_name = opts.key_name.clone().unwrap_or_else(default_key_name);
            let result = get_or_create_api_key(&server, &c, &key_name).await?;
            println!(
                "• {} API key \"{key_name}\" ({})",
                if result.created { "Created" } else { "Reusing" },
                mask_key(&result.key)
            );
            api_key = result.key;
            cookie = Some(c);
        }
    };

    // Models come from the server's own cli-tools config (best-effort); flags override.
    let server_models = fetch_server_models(&server, cookie.as_deref(), &tools).await;
    if opts.api_key.is_some() {
        println!("• --api-key given: can't read the server's models (needs a dashboard login) — pass model flags");
    }

    let available = list_models(&server, &api_key).await;

    let warn_missing = |label: &str, model: &str, flag: &str| {
        if let Some(set) = &available {
            let bare = model.strip_suffix("[1m]").unwrap_or(model);
            if !set.contains(bare) {
                warn(&format!("{label}: \"{model}\" not listed by server — override with {flag} <model>"));
            }
        }
    };

    // Claude tier: flag > server value > unset (Claude Code then uses its own default).
    let mut claude_models: BTreeMap<String, String> = BTreeMap::new();
    let mut claude_unset: Vec<&str> = Vec::new();
    if tools.iter().any(|t| t.id() == "claude") {
        for (flag, env_key) in connect_tools::CLAUDE_MODELS {
            let model = server_models.claude.get(env_key).cloned().or_else(|| server_models.claude.get(flag).cloned());
            match model {
                Some(m) => {
                    warn_missing(&format!("claude {flag}"), &m, &format!("--{flag}"));
                    claude_models.insert(env_key.into(), m);
                }
                None => claude_unset.push(flag),
            }
        }
    }

    // Other tools: --model > that tool's own server model > server's OpenCode model.
    let mut tool_models: BTreeMap<String, String> = BTreeMap::new();
    let mut from_opencode: Vec<&str> = Vec::new();
    for t in &tools {
        if !connect_tools::needs_model(t.as_ref()) {
            continue;
        }
        let model = opts
            .model
            .clone()
            .or_else(|| server_models.by_tool.get(t.id()).cloned())
            .or_else(|| server_models.shared.clone());
        if !opts.model.is_some() && server_models.by_tool.get(t.id()).is_none() && server_models.shared.is_some() {
            from_opencode.push(t.id());
        }
        if let Some(m) = model {
            warn_missing("model", &m, "--model");
            tool_models.insert(t.id().into(), m);
        }
    }
    if !from_opencode.is_empty() {
        println!(
            "• {}: no own model on the server — using its OpenCode model",
            from_opencode.join(", ")
        );
    }

    let mut failed = 0usize;
    let mut skipped = 0usize;
    for t in &tools {
        let needs = connect_tools::needs_model(t.as_ref());
        let model = tool_models.get(t.id()).cloned();
        if needs && model.is_none() {
            skipped += 1;
            println!("⏭  {}: no model configured on the server — skipped (pass --model <model>)", t.name());
            continue;
        }
        let ctx = ApplyCtx {
            base_url: server.clone(),
            api_key: api_key.clone(),
            model,
            claude_models: claude_models.clone(),
        };
        match t.apply(&home_dir(), &ctx) {
            Ok(files) => {
                let names: Vec<String> = files.iter().map(|f| f.display().to_string()).collect();
                println!("✅ {} → {}", t.name(), names.join(", "));
            }
            Err(e) => {
                failed += 1;
                println!("❌ {}: {e}", t.name());
            }
        }
    }

    println!("   Base URL: {server}/v1");
    if tools.iter().any(|t| t.id() == "claude") {
        for (_, env_key) in connect_tools::CLAUDE_MODELS {
            if let Some(m) = claude_models.get(env_key) {
                println!("   {env_key}={m}");
            }
        }
        if !claude_unset.is_empty() {
            println!(
                "   Claude {}: not set on the server — Claude Code's own default applies (or pass --{} <model>)",
                claude_unset.join(", "),
                claude_unset[0]
            );
        }
    }
    for (id, m) in &tool_models {
        println!("   {id} model: {m}");
    }
    if tools.iter().any(|t| t.id() == "omp") {
        println!("   omp models: every server model, listed under 9router in /model");
    }
    let ids: Vec<&str> = tools.iter().map(|t| t.id()).collect();
    println!("   Restart the tools to apply. Undo: npx 9router connect --reset --tools {}", ids.join(","));

    if opts.print_env {
        println!("\nOpenAI-compatible CLIs (codex, opencode, aider, …):");
        println!("   OPENAI_BASE_URL={server}/v1");
        println!("   OPENAI_API_KEY={api_key}");
    }

    if failed > 0 || skipped > 0 {
        return Err(anyhow!("{failed} failed, {skipped} skipped"));
    }
    Ok(())
}

fn run_reset(tools: &[Box<dyn ToolConfig>]) -> Result<()> {
    let mut failed = 0usize;
    for t in tools {
        match t.reset(&home_dir()) {
            Ok(files) => {
                if files.is_empty() {
                    println!("• {}: nothing to reset", t.name());
                } else {
                    let names: Vec<String> = files.iter().map(|f| f.display().to_string()).collect();
                    println!("✅ {}: removed 9router settings ({})", t.name(), names.join(", "));
                }
            }
            Err(e) => {
                failed += 1;
                println!("❌ {}: {e}", t.name());
            }
        }
    }
    if failed > 0 {
        return Err(anyhow!("{failed} failed"));
    }
    Ok(())
}

fn select_tools(opts: &ConnectOpts) -> Result<Vec<Box<dyn ToolConfig>>> {
    if let Some(list) = &opts.tools {
        return connect_tools::resolve_tools(list);
    }
    if !stdin_is_tty() {
        return connect_tools::resolve_tools(&["claude".into()]);
    }
    let all = connect_tools::tools();
    let items: Vec<&str> = all.iter().map(|t| t.name()).collect();
    let chosen = Select::new("Select CLI tool to configure", items.clone()).prompt()?;
    let idx = items.iter().position(|n| *n == chosen).unwrap_or(0);
    Ok(vec![all.into_iter().nth(idx).unwrap()])
}

fn prompt_password() -> Result<String> {
    if !stdin_is_tty() {
        return Err(anyhow!("Password required: pass --password or set NINE_ROUTER_PASSWORD"));
    }
    let pw = Password::new("9router dashboard password").prompt()?;
    Ok(pw)
}

/// Read the password saved by an earlier `--save` run, only if it belongs to
/// this exact server (a password must never be sent to a different host).
fn load_saved_password(server: &str) -> Option<String> {
    let text = fs::read_to_string(saved_env_path()).ok()?;
    let mut saved_server: Option<String> = None;
    let mut saved_password: Option<String> = None;
    for line in text.lines() {
        let Some((key, raw)) = line.split_once('=') else { continue };
        // Values are written JSON-quoted; accept a hand-edited bare value too.
        let value = serde_json::from_str::<String>(raw).unwrap_or_else(|_| raw.to_string());
        match key {
            "NINE_ROUTER_SERVER" => saved_server = Some(value),
            "NINE_ROUTER_PASSWORD" => saved_password = Some(value),
            _ => {}
        }
    }
    if saved_server.as_deref() == Some(server) {
        saved_password
    } else {
        None
    }
}

fn stdin_is_tty() -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

// ── Network: login, API keys, server models ──────────────────────────────────

async fn request(
    url: &str,
    method: &str,
    body: Option<&Value>,
    cookie: Option<&str>,
    api_key: Option<&str>,
) -> Result<(u16, Option<Value>)> {
    let client = reqwest::Client::new();
    let mut req = client
        .request(
            method.parse().unwrap_or(reqwest::Method::GET),
            reqwest::Url::parse(url).map_err(|e| anyhow!("Bad URL {url}: {e}"))?,
        )
        .header("Accept", "application/json");
    if let Some(b) = body {
        req = req.header("Content-Type", "application/json").json(b);
    }
    if let Some(c) = cookie {
        req = req.header("Cookie", c);
    }
    if let Some(k) = api_key {
        req = req.bearer_auth(k);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| anyhow!("Cannot reach {url}: {e}"))?;
    let status = resp.status().as_u16();
    // Server-controlled strings get printed later — error is stripped of control chars.
    let data = resp.json::<Value>().await.ok();
    Ok((status, data))
}

async fn login(server: &str, password: &str) -> Result<String> {
    let client = reqwest::Client::new();
    let url = format!("{server}/api/auth/login");
    let resp = client
        .post(&url)
        .json(&json!({"password": password}))
        .send()
        .await
        .map_err(|e| anyhow!("Cannot reach {server}: {e}"))?;
    let status = resp.status().as_u16();
    let cookie = resp
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|c| {
            // auth_token=<value>;…
            c.split(';')
                .next()
                .and_then(|pair| pair.trim().strip_prefix("auth_token="))
                .map(|v| format!("auth_token={v}"))
        });
    let data: Value = resp.json().await.unwrap_or(Value::Null);
    if status == 200 && data.get("success").and_then(|v| v.as_bool()) == Some(true) {
        return cookie.ok_or_else(|| anyhow!("Login succeeded but server returned no session cookie"));
    }
    let err = data.get("error").and_then(|v| v.as_str()).unwrap_or("unknown error");
    Err(anyhow!("Login failed ({status}): {err}"))
}

async fn get_or_create_api_key(server: &str, cookie: &str, key_name: &str) -> Result<ApiKeyResult> {
    let (status, data) = request(&format!("{server}/api/keys"), "GET", None, Some(cookie), None).await?;
    if status == 401 {
        return Err(anyhow!("Unauthorized listing API keys — wrong password or session rejected"));
    }
    if status != 200 {
        let err = data.and_then(|d| d.get("error").and_then(|v| v.as_str()).map(String::from)).unwrap_or_default();
        return Err(anyhow!("Failed to list API keys ({status}): {err}"));
    }
    let keys = data
        .and_then(|d| d.get("keys").and_then(|k| k.as_array()).cloned())
        .unwrap_or_default();
    let active: Vec<&Value> = keys.iter().filter(|k| k.get("isActive").and_then(|v| v.as_bool()) != Some(false)).collect();
    if let Some(existing) = active.iter().find(|k| k.get("name").and_then(|v| v.as_str()) == Some(key_name)) {
        let key = existing.get("key").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("key entry missing `key`"))?;
        return Ok(ApiKeyResult { key: key.into(), created: false });
    }

    let (status, data) = request(
        &format!("{server}/api/keys"),
        "POST",
        Some(&json!({"name": key_name})),
        Some(cookie),
        None,
    )
    .await?;
    if status != 201 {
        let err = data.and_then(|d| d.get("error").and_then(|v| v.as_str()).map(String::from)).unwrap_or_default();
        return Err(anyhow!("Failed to create API key ({status}): {err}"));
    }
    let key = data
        .and_then(|d| d.get("key").and_then(|v| v.as_str()).map(String::from))
        .ok_or_else(|| anyhow!("Failed to create API key ({status}): response missing key"))?;
    Ok(ApiKeyResult { key, created: true })
}

struct ApiKeyResult {
    key: String,
    created: bool,
}

#[derive(Default)]
struct ServerModels {
    claude: BTreeMap<String, String>,
    by_tool: BTreeMap<String, String>,
    shared: Option<String>,
}

/// Model choices the server operator already made, read from the server's own
/// cli-tools config. Only model ids are taken — never the server's baseUrl or
/// apiKey, which belong to that host. Best-effort: any failure keeps defaults.
async fn fetch_server_models(
    server: &str,
    cookie: Option<&str>,
    tools: &[Box<dyn ToolConfig>],
) -> ServerModels {
    let mut out = ServerModels::default();
    // Only readable with a dashboard login (no cookie when --api-key was given).
    let Some(cookie) = cookie else { return out };

    if tools.iter().any(|t| t.id() == "claude") {
        let url = format!("{server}/api/cli-tools/claude-settings");
        if let Ok((200, Some(data))) = request(&url, "GET", None, Some(cookie), None).await {
            for (_, env_key) in connect_tools::CLAUDE_MODELS {
                let path = format!("/settings/env/{env_key}");
                if let Some(m) = data.pointer(&path).and_then(|v| v.as_str()) {
                    if !m.is_empty() {
                        out.claude.insert(env_key.into(), m.into());
                    }
                }
            }
        }
    }

    // Each selected tool with a route, plus OpenCode (the shared fallback).
    let all = connect_tools::tools();
    let mut targets: Vec<&dyn ToolConfig> = tools.iter().map(|t| t.as_ref()).collect();
    if !targets.iter().any(|t| t.id() == "opencode")
        && tools.iter().any(|t| connect_tools::needs_model(t.as_ref()))
    {
        if let Some(oc) = all.iter().find(|t| t.id() == "opencode") {
            targets.push(oc.as_ref());
        }
    }

    for t in targets {
        let Some(route) = t.route() else { continue };
        let url = format!("{server}/api/cli-tools/{route}");
        if let Ok((200, data)) = request(&url, "GET", None, Some(cookie), None).await {
            if let Some(model) = t.server_model(&data.unwrap_or(Value::Null)) {
                out.by_tool.insert(t.id().into(), model);
            }
        }
    }
    out.shared = out.by_tool.get("opencode").cloned();
    out
}

async fn list_models(server: &str, api_key: &str) -> Option<std::collections::HashSet<String>> {
    let (status, data) = request(&format!("{server}/v1/models"), "GET", None, None, Some(api_key)).await.ok()?;
    if status == 401 {
        println!("⚠ API key rejected by server (/v1/models returned 401)");
        return None;
    }
    if status != 200 {
        return None;
    }
    let ids = data?
        .get("data")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|m| m.get("id").and_then(|v| v.as_str())).map(String::from).collect())?;
    Some(ids)
}

// ── Saved login (--save) ─────────────────────────────────────────────────────

fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("DATA_DIR") {
        return PathBuf::from(d);
    }
    nr_core::paths::data_dir().to_path_buf()
}

fn saved_env_path() -> PathBuf {
    data_dir().join("connect.env")
}

fn save_password(server: &str, password: &str) -> Result<()> {
    let file = saved_env_path();
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    let body = format!(
        "NINE_ROUTER_SERVER={}\nNINE_ROUTER_PASSWORD={}\n",
        json_quoted(server),
        json_quoted(password)
    );
    let mut f = fs::File::create(&file)?;
    f.write_all(body.as_bytes())?;
    set_mode_600(&file);
    Ok(())
}

fn json_quoted(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
}

#[cfg(unix)]
fn set_mode_600(file: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = fs::metadata(file) {
        let mut perms = meta.permissions();
        perms.set_mode(0o600);
        let _ = fs::set_permissions(file, perms);
    }
}

#[cfg(not(unix))]
fn set_mode_600(_file: &std::path::Path) {}

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

// ── show ─────────────────────────────────────────────────────────────────────

fn handle_show(tools: &[String]) -> Result<()> {
    let help = tools.iter().any(|t| t == "-h" || t == "--help");
    let json_output = tools.iter().any(|t| t == "--json");
    if help {
        print!("{HELP_SHOW}");
        return Ok(());
    }
    let mut names: Vec<String> = Vec::new();
    let mut i = 0;
    while i < tools.len() {
        match tools[i].as_str() {
            "--tools" => {
                i += 1;
                if let Some(v) = tools.get(i) {
                    names.extend(v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
                }
            }
            a if a.starts_with('-') => {} // flags already handled above
            a => names.push(a.to_string()),
        }
        i += 1;
    }
    let resolved = if names.is_empty() {
        connect_tools::tools()
    } else {
        // `--tools a,b` was already flattened into `names` above.
        connect_tools::resolve_tools(&names)?
    };

    let mut records: Vec<Value> = Vec::new();
    let mut had_error = false;
    for t in &resolved {
        let info = match t.show(&home_dir()) {
            Ok(i) => i,
            Err(e) => {
                had_error = true;
                records.push(json!({"id": t.id(), "name": t.name(), "files": paths_of(t), "error": e.to_string()}));
                continue;
            }
        };
        records.push(record_json(t, &info));
    }

    if json_output {
        println!("{}", serde_json::to_string_pretty(&records)?);
    } else {
        for (i, r) in records.iter().enumerate() {
            if i > 0 {
                println!();
            }
            print_record(r);
        }
    }
    if had_error {
        return Err(anyhow!("one or more tools could not be read"));
    }
    Ok(())
}

fn paths_of(t: &dyn ToolConfig) -> Vec<String> {
    t.paths(&home_dir()).iter().map(|p| p.display().to_string()).collect()
}

fn record_json(t: &dyn ToolConfig, info: &connect_tools::ShowInfo) -> Value {
    let mut r = json!({
        "id": t.id(),
        "name": t.name(),
        "files": paths_of(t),
        "configured": info.is_configured(),
        "baseUrl": info.base_url.clone(),
        "apiKey": info.api_key.as_deref().map(mask_key),
        "models": info.models,
    });
    if let Some(active) = info.active {
        r["active"] = json!(active);
    }
    r
}

fn print_record(r: &Value) {
    if let Some(err) = r.get("error").and_then(|v| v.as_str()) {
        println!("❌ {}: {err}", r["name"].as_str().unwrap_or("unknown"));
        return;
    }
    let name = r["name"].as_str().unwrap_or("unknown");
    let configured = r.get("configured").and_then(|v| v.as_bool()).unwrap_or(false);
    let files = r["files"].as_array().map(|a| {
        a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", ")
    }).unwrap_or_default();
    if !configured {
        println!("• {name}: not configured for 9router ({files})");
        return;
    }
    let inactive = if r.get("active").and_then(|v| v.as_bool()) == Some(false) {
        "  \u{1b}[33m(9router present but not the active provider)\u{1b}[0m"
    } else {
        ""
    };
    println!("✅ {name}{inactive}");
    println!("   File:     {files}");
    println!("   Base URL: {}", r["baseUrl"].as_str().unwrap_or("(none)"));
    println!("   API key:  {}", r["apiKey"].as_str().unwrap_or("(none)"));
    let models = r["models"].as_object();
    let entries: Vec<(&String, &Value)> = models.map(|m| m.iter().collect()).unwrap_or_default();
    if entries.is_empty() {
        println!("   Models:   (none set — the tool's own default applies)");
    } else {
        let width = entries.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
        println!("   Models:");
        for (k, v) in entries {
            println!("     {k:<width$}  {}", v.as_str().unwrap_or(""));
        }
    }
}

// ── xai video ────────────────────────────────────────────────────────────────

/// `9router xai video …` — port of cli/src/cli/commands/xaiVideo.js.
///
/// The HTTP calls block, so they run on a blocking thread instead of stalling
/// the tokio runtime that also serves the launcher.
async fn handle_xai(cmd: XaiCommand) -> Result<()> {
    match cmd {
        XaiCommand::Video { args } => {
            let code = tokio::task::spawn_blocking(move || xai_video::run(&args))
                .await
                .unwrap_or(1);
            if code == 0 {
                Ok(())
            } else {
                Err(anyhow!("xai video exited with code {code}"))
            }
        }
    }
}

// ── update ───────────────────────────────────────────────────────────────────

const HELP_UPDATE: &str = r#"
Usage: 9router update [options]

Check GitHub releases for a newer version and offer to self-update.

Options:
  --check       Only report; never prompt or install
  -h, --help    Show this help
"#;

const RELEASES_API: &str = "https://api.github.com/repos/cabrata/9router-rust/releases/latest";

/// Check GitHub releases for a newer version. Returns the latest version string
/// when it is newer than `current`, else None. Never panics; network failures
/// return None so callers keep running.
pub async fn check_for_update(current: &str) -> Option<String> {
    let client = reqwest::Client::new();
    let resp = client
        .get(RELEASES_API)
        .header("Accept", "application/vnd.github.v3+json")
        .header("User-Agent", "9router-cli")
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: Value = resp.json().await.ok()?;
    let tag = json.get("tag_name").and_then(|v| v.as_str())?;
    let latest = tag.trim_start_matches('v');
    if compare_versions(latest, current) > 0 {
        Some(latest.to_string())
    } else {
        None
    }
}

/// Compare two dotted versions. Returns >0 if a > b, <0 if a < b, 0 if equal.
fn compare_versions(a: &str, b: &str) -> i32 {
    let parse = |s: &str| -> Vec<u64> {
        s.split('.').map(|p| p.parse().unwrap_or(0)).collect()
    };
    let pa = parse(a);
    let pb = parse(b);
    let len = pa.len().max(pb.len());
    for i in 0..len {
        let x = pa.get(i).copied().unwrap_or(0);
        let y = pb.get(i).copied().unwrap_or(0);
        if x > y {
            return 1;
        }
        if x < y {
            return -1;
        }
    }
    0
}

async fn handle_update(extra: &[String]) -> Result<()> {
    let check_only = extra.iter().any(|a| a == "--check");
    if extra.iter().any(|a| a == "-h" || a == "--help") {
        print!("{HELP_UPDATE}");
        return Ok(());
    }
    let current = env!("CARGO_PKG_VERSION");
    println!("\n⬆  Checking for updates… (current: v{current})");
    match check_for_update(current).await {
        Some(latest) => {
            println!("   Latest release: v{latest}");
            if check_only {
                return Ok(());
            }
            // Self-update via cargo if the binary was built from source (most
            // likely for this crate); an installed binary should use its installer.
            if !stdin_is_tty() {
                println!("   Run `cargo install --path crates/nr-cli` to update, or reinstall via your installer.");
                return Ok(());
            }
            let do_update = inquire::Confirm::new("Update now?")
                .with_default(true)
                .prompt()
                .unwrap_or(false);
            if !do_update {
                println!("   Update skipped.");
                return Ok(());
            }
            println!("   Updating via cargo…");
            let status = std::process::Command::new("cargo")
                .args(["install", "--path", "crates/nr-cli"])
                .status()
                .map_err(|e| anyhow!("Failed to run cargo: {e}"))?;
            if status.success() {
                println!("✅ Updated to v{latest}. Restart 9router to apply.");
            } else {
                return Err(anyhow!("cargo install failed with {status}"));
            }
        }
        None => {
            if check_only {
                println!("   Already up to date (or could not reach GitHub).\n");
            } else {
                println!("   ✅ Already up to date (v{current}).");
            }
        }
    }
    Ok(())
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Normalize server URL: add http:// if missing and keep only the origin.
pub fn normalize_server_url(url: &str) -> String {
    let raw = url.trim();
    let with_scheme = if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    // Keep only scheme://host[:port]
    if let Some(rest) = with_scheme.strip_prefix("http://") {
        let host = rest.split(['/', '?', '#']).next().unwrap_or("");
        return format!("http://{host}");
    }
    if let Some(rest) = with_scheme.strip_prefix("https://") {
        let host = rest.split(['/', '?', '#']).next().unwrap_or("");
        return format!("https://{host}");
    }
    with_scheme
}

/// Mask API key for display (first 6 and last 4 chars).
fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() < 12 {
        "****".to_string()
    } else {
        let head: String = chars[..6].iter().collect();
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("{head}…{tail}")
    }
}

fn warn(msg: &str) {
    println!("\u{1b}[33m⚠ {msg}\u{1b}[0m");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_url_keeps_only_origin() {
        assert_eq!(normalize_server_url("localhost:20128"), "http://localhost:20128");
        assert_eq!(normalize_server_url("http://host:20128/dashboard"), "http://host:20128");
        assert_eq!(normalize_server_url("https://host/x?y=1"), "https://host");
        assert_eq!(normalize_server_url("  http://h:1/v1  "), "http://h:1");
    }

    #[test]
    fn mask_key_hides_the_middle() {
        assert_eq!(mask_key("short"), "****");
        assert_eq!(mask_key("sk-abcdef1234567890"), "sk-abc…7890");
        // Multi-byte input must not panic on a byte slice boundary.
        let masked = mask_key("ключ-abcdefghijkl");
        assert!(masked.contains('…'));
    }

    #[test]
    fn compare_versions_orders_numerically() {
        assert_eq!(compare_versions("0.2.0", "0.1.9"), 1);
        assert_eq!(compare_versions("0.1.0", "0.2.0"), -1);
        assert_eq!(compare_versions("1.0.0", "1.0.0"), 0);
        assert_eq!(compare_versions("1.0", "1.0.0"), 0);
        assert_eq!(compare_versions("1.10.0", "1.9.0"), 1, "numeric, not lexicographic");
    }

    #[test]
    fn parse_connect_args_reads_every_flag() {
        let extra: Vec<String> = [
            "--tools", "claude,codex", "--password", "pw", "--api-key", "sk-x",
            "--model", "m1", "--save", "--print-env", "--reset", "--key-name", "kn",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let opts = parse_connect_args("host:20128", &extra).unwrap();
        assert_eq!(opts.url.as_deref(), Some("host:20128"));
        assert_eq!(opts.tools.as_deref(), Some(&["claude".to_string(), "codex".to_string()][..]));
        assert_eq!(opts.password.as_deref(), Some("pw"));
        assert_eq!(opts.api_key.as_deref(), Some("sk-x"));
        assert_eq!(opts.model.as_deref(), Some("m1"));
        assert_eq!(opts.key_name.as_deref(), Some("kn"));
        assert!(opts.save && opts.print_env && opts.reset);
    }

    #[test]
    fn parse_connect_args_rejects_unknown_and_missing_values() {
        assert!(parse_connect_args("h", &["--bogus".to_string()]).is_err());
        assert!(parse_connect_args("h", &["--tools".to_string()]).is_err());
    }

    #[test]
    fn saved_password_is_bound_to_its_server() {
        // Uses a temp DATA_DIR so the real ~/.9router is never touched. This test
        // mutates a process-wide env var, so hold a static lock across the run.
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("DATA_DIR", dir.path());
        let server = "http://one:20128";
        save_password(server, "pw with \"quotes\"\nand newline").unwrap();
        assert_eq!(
            load_saved_password(server).as_deref(),
            Some("pw with \"quotes\"\nand newline")
        );
        assert_eq!(load_saved_password("http://other:20128"), None, "never sent to another host");
        std::env::remove_var("DATA_DIR");
    }
}
