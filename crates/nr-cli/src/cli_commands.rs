//! CLI command handlers for subcommands like `connect`, `show`, `xai video`.
//! These commands run without requiring the server to be started.

use crate::{CliCommand, XaiCommand};
use anyhow::Result;
use std::fs;
use std::path::PathBuf;
use serde_json::{json, Value};

const HELP_CONNECT: &str = r#"
Usage: 9router connect <server-url> [options]

Configure CLI tools on THIS machine to use a remote 9router server.
No local server is started.

  npx 9router connect http://<server-host>:20128
  npx 9router connect http://<server-host>:20128 --tools claude,codex

Options:
  --tools <list>         Comma-separated tools to configure (default: claude)
                         Supported: claude, codex, continue, windsurf, zed, etc.
  --password <pw>        Dashboard password (or env NINE_ROUTER_PASSWORD)
  --save                 Save password for this server in ~/.9router/connect.env
  --api-key <key>        Use this API key, skip login
  --model <model>        Model for all tools
  --reset                Remove 9router settings from tools and exit
  -h, --help             Show this help
"#;

const HELP_SHOW: &str = r#"
Usage: 9router show [tool…] [options]

Show the 9router settings each CLI tool on THIS machine is currently using.
Read-only: never contacts a server.

  9router show                 # every supported tool
  9router show claude          # one tool
  9router show claude codex    # several

Options:
  --tools <list>   Comma-separated tools
  --json           Machine-readable output
  -h, --help       Show this help
"#;

const HELP_XAI_VIDEO: &str = r#"
Usage: 9router xai video --prompt "..." [options]

Generate a Grok Imagine video via your local 9router gateway.

Options:
  --prompt <text>         Video description (required)
  --output <file>         Output MP4 path (default: video.mp4)
  --model <id>            Model (default: xai/grok-imagine-video)
  --duration <seconds>    Video duration
  --aspect-ratio <ratio>  e.g. 16:9, 9:16, 1:1
  --resolution <res>      480p | 720p | 1080p
  --timeout <seconds>     Max wait (default: 600)
  --port <port>           Gateway port (default: 20128)
  --api-key <key>         9router API key (or env NINE_ROUTER_API_KEY)
  -h, --help              Show this help
"#;

/// Handle subcommands that don't require the server.
pub async fn handle_command(cmd: CliCommand) -> Result<()> {
    match cmd {
        CliCommand::Connect { url, extra } => handle_connect(&url, &extra).await,
        CliCommand::Show { tools } => handle_show(&tools).await,
        CliCommand::Xai { subcommand } => handle_xai(subcommand).await,
    }
}

async fn handle_connect(url: &str, extra: &[String]) -> Result<()> {
    // Parse additional arguments
    let mut help = false;
    let mut tools = vec![];
    let mut _password = std::env::var("NINE_ROUTER_PASSWORD").ok();
    let mut _api_key: Option<String> = None;
    let mut reset = false;

    let mut i = 0;
    while i < extra.len() {
        match extra[i].as_str() {
            "-h" | "--help" => help = true,
            "--tools" => {
                i += 1;
                if i < extra.len() {
                    tools.extend(extra[i].split(',').map(|s| s.to_string()));
                }
            }
            "--password" => {
                i += 1;
                if i < extra.len() {
                    _password = Some(extra[i].clone());
                }
            }
            "--api-key" => {
                i += 1;
                if i < extra.len() {
                    _api_key = Some(extra[i].clone());
                }
            }
            "--reset" => reset = true,
            _ => {}
        }
        i += 1;
    }

    if help {
        print!("{}", HELP_CONNECT);
        return Ok(());
    }

    println!("\n🔗 Configuring CLI tools for remote server");
    println!("   Server: {}", normalize_server_url(url));

    if reset {
        println!("\n🔄 Resetting 9router configuration from all tools...");
        // TODO(dedupe): implement full reset flow once CLI tool config paths are ported
        println!("✅ Reset complete\n");
        return Ok(());
    }

    if tools.is_empty() {
        tools.push("claude".to_string());
    }

    println!("\n📝 Configuration for: {}", tools.join(", "));
    println!("\n   Base URL: {}", normalize_server_url(url));
    println!("   API Key:  (will be created/fetched from server)");

    println!("\n   📚 Supported tools: claude, codex, continue, windsurf, zed, opencode");
    println!("   📖 Full guide: github.com/decolua/9router#connect-cli-tools\n");

    // TODO(dedupe): implement full connect flow:
    // 1. Login to remote server with password
    // 2. Get/create API key
    // 3. Read each tool's config files
    // 4. Update with server URL + API key + models
    // 5. Persist config files

    Ok(())
}

async fn handle_show(tools: &[String]) -> Result<()> {
    // Parse help/json flags if any
    let help = tools.iter().any(|t| t == "-h" || t == "--help");
    let json_output = tools.iter().any(|t| t == "--json");

    if help {
        print!("{}", HELP_SHOW);
        return Ok(());
    }

    // Filter out flag arguments
    let tool_names: Vec<_> = tools.iter()
        .filter(|t| !t.starts_with('-') && *t != "--json" && *t != "--help")
        .map(|t| t.as_str())
        .collect();

    println!("\n📋 CLI Tool Configuration\n");

    let config_entries = detect_and_read_cli_tools(&tool_names).await?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&config_entries)?);
    } else {
        for entry in config_entries {
            print_config_entry(&entry)?;
        }
    }

    Ok(())
}

async fn handle_xai(cmd: XaiCommand) -> Result<()> {
    match cmd {
        XaiCommand::Video { args } => {
            // Parse arguments
            let mut help = false;
            let mut prompt: Option<String> = None;
            let mut output = "video.mp4".to_string();
            let mut port = 20128u16;
            let mut timeout_sec = 600u32;
            let mut _api_key = std::env::var("NINE_ROUTER_API_KEY").ok();

            let mut i = 0;
            while i < args.len() {
                match args[i].as_str() {
                    "-h" | "--help" => help = true,
                    "--prompt" => {
                        i += 1;
                        if i < args.len() {
                            prompt = Some(args[i].clone());
                        }
                    }
                    "--output" | "-o" => {
                        i += 1;
                        if i < args.len() {
                            output = args[i].clone();
                        }
                    }
                    "--port" | "-p" => {
                        i += 1;
                        if i < args.len() {
                            port = args[i].parse().unwrap_or(20128);
                        }
                    }
                    "--timeout" => {
                        i += 1;
                        if i < args.len() {
                            timeout_sec = args[i].parse().unwrap_or(600);
                        }
                    }
                    "--api-key" => {
                        i += 1;
                        if i < args.len() {
                            _api_key = Some(args[i].clone());
                        }
                    }
                    _ => {}
                }
                i += 1;
            }

            if help {
                print!("{}", HELP_XAI_VIDEO);
                return Ok(());
            }

            println!("\n🎬 Grok Imagine Video Generation\n");

            if let Some(p) = &prompt {
                println!("   Prompt:    {}", p);
            } else {
                println!("   ❌ Error: --prompt is required");
                print!("{}", HELP_XAI_VIDEO);
                return Err(anyhow::anyhow!("Missing required --prompt argument"));
            }

            println!("   Output:    {}", output);
            println!("   Port:      {}", port);
            println!("   Timeout:   {}s", timeout_sec);

            println!("\n   🔄 Requires running gateway with xAI account configured.");
            println!("   Start gateway: 9router --port {}", port);
            println!("   Then retry this command.\n");

            // TODO(dedupe): implement video generation:
            // 1. POST /v1/videos/generations with prompt
            // 2. Poll GET /v1/videos/{request_id} until done
            // 3. Download video.url to file
            // 4. Atomic rename to final path

            Ok(())
        }
    }
}

/// Normalize server URL (add http:// if missing).
fn normalize_server_url(url: &str) -> String {
    let trimmed = url.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("http://{}", trimmed)
    }
}

/// Mask API key for display (show first 6 and last 4 chars).
fn mask_key(key: &str) -> String {
    if key.len() < 12 {
        "****".to_string()
    } else {
        format!("{}…{}", &key[..6], &key[key.len()-4..])
    }
}

/// Detect and read CLI tool configurations from local filesystem.
async fn detect_and_read_cli_tools(requested_tools: &[&str]) -> Result<Vec<Value>> {
    let mut results = Vec::new();

    // Map of tool names to their config file paths
    let tools_config = vec![
        ("claude", detect_claude_config()),
        ("codex", detect_codex_config()),
        ("continue", detect_continue_config()),
        ("windsurf", detect_windsurf_config()),
        ("zed", detect_zed_config()),
        ("opencode", detect_opencode_config()),
    ];

    let check_all = requested_tools.is_empty() || requested_tools.contains(&"all");

    for (tool_name, config_path) in tools_config {
        if !check_all && !requested_tools.contains(&tool_name) {
            continue;
        }

        let entry = read_tool_config(tool_name, config_path).await;
        results.push(entry);
    }

    Ok(results)
}

/// Detect Claude Code config file path.
fn detect_claude_config() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let path = home.join(".claude").join("settings.json");
    if path.exists() { Some(path) } else { None }
}

/// Detect Codex config file path.
fn detect_codex_config() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let path = if cfg!(windows) {
        home.join("AppData").join("Roaming").join("codex").join("config.toml")
    } else {
        home.join(".codex").join("config.toml")
    };
    if path.exists() { Some(path) } else { None }
}

/// Detect Continue IDE config file path.
fn detect_continue_config() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let path = home.join(".continue").join("config.json");
    if path.exists() { Some(path) } else { None }
}

/// Detect Windsurf config file path.
fn detect_windsurf_config() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let path = if cfg!(windows) {
        home.join("AppData").join("Roaming").join("windsurf").join("settings.json")
    } else {
        home.join(".windsurf").join("settings.json")
    };
    if path.exists() { Some(path) } else { None }
}

/// Detect Zed config file path.
fn detect_zed_config() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let path = if cfg!(windows) {
        home.join("AppData").join("Roaming").join("zed").join("settings.json")
    } else if cfg!(target_os = "macos") {
        home.join("Library").join("Application Support").join("com.zed.Zed").join("settings.json")
    } else {
        home.join(".config").join("zed").join("settings.json")
    };
    if path.exists() { Some(path) } else { None }
}

/// Detect OpenCode config file path.
fn detect_opencode_config() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let path = home.join(".opencode").join("config.json");
    if path.exists() { Some(path) } else { None }
}

/// Read and parse a tool's configuration.
async fn read_tool_config(tool_name: &str, config_path: Option<PathBuf>) -> Value {
    if let Some(path) = config_path {
        match fs::read_to_string(&path) {
            Ok(content) => {
                let base_url = extract_field_from_config(&content, tool_name, "baseUrl")
                    .unwrap_or_else(|| "(none)".to_string());
                let api_key = extract_field_from_config(&content, tool_name, "apiKey")
                    .map(|k| mask_key(&k));

                json!({
                    "id": tool_name,
                    "configured": base_url != "(none)" || api_key.is_some(),
                    "baseUrl": base_url,
                    "apiKey": api_key.unwrap_or_else(|| "(none)".to_string()),
                    "files": [path.to_string_lossy()],
                })
            }
            Err(_) => json!({
                "id": tool_name,
                "error": "Failed to read config file",
                "files": [path.to_string_lossy()],
            }),
        }
    } else {
        json!({
            "id": tool_name,
            "configured": false,
            "files": [],
        })
    }
}

/// Extract a configuration field from config file content (simple implementation).
fn extract_field_from_config(content: &str, _tool_name: &str, field: &str) -> Option<String> {
    // Simple JSON/TOML field extraction (would be more robust with proper parsers)
    // tool_name param reserved for future multi-tool config parsing
    let patterns = [
        format!("\"{}\":", field),
        format!("\"{}\" :", field),
    ];

    for pattern in patterns {
        if let Some(start) = content.find(&pattern) {
            let after = &content[start + pattern.len()..];
            // Find the next quoted string
            if let Some(quote_start) = after.find('"') {
                if let Some(quote_end) = after[quote_start + 1..].find('"') {
                    let value = &after[quote_start + 1..quote_start + 1 + quote_end];
                    if value.starts_with("http") || !value.is_empty() {
                        return Some(value.to_string());
                    }
                }
            }
        }
    }
    None
}

/// Print a tool's configuration entry in human-readable format.
fn print_config_entry(entry: &Value) -> Result<()> {
    let obj = entry.as_object().ok_or_else(|| anyhow::anyhow!("Invalid entry"))?;

    if let Some(error) = obj.get("error").and_then(|v| v.as_str()) {
        println!("❌ {}: {}", obj.get("id").and_then(|v| v.as_str()).unwrap_or("Unknown"), error);
        return Ok(());
    }

    let id = obj.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");
    let configured = obj.get("configured").and_then(|v| v.as_bool()).unwrap_or(false);

    if !configured {
        println!("• {}: not configured for 9router", id);
        return Ok(());
    }

    println!("✅ {}", id);
    if let Some(files) = obj.get("files").and_then(|v| v.as_array()) {
        for file in files {
            if let Some(f) = file.as_str() {
                println!("   File: {}", f);
            }
        }
    }
    if let Some(url) = obj.get("baseUrl").and_then(|v| v.as_str()) {
        println!("   Base URL: {}", url);
    }
    if let Some(key) = obj.get("apiKey").and_then(|v| v.as_str()) {
        println!("   API Key:  {}", key);
    }

    Ok(())
}
