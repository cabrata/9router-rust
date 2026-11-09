//! The `9router` command. Port of cli/cli.js (owner: cli agent, see AGENTS.md).
//!
//! Manages the lifecycle of the 9router server: spawning it, tracking via PID file,
//! presenting interactive menus, and handling graceful shutdown.

use clap::Parser;
use nr_cli::launcher_process::{get_pid_file_path, read_pid_record, remove_pid_record_if_owner, write_pid_record, PidRecord};
use nr_cli::port_management::wait_for_server_ready;
use nr_cli::process_management::stop_recorded_instance;
use nr_cli::ui::{clear_screen, warn};
use nr_core::paths;
use std::env;
use std::fs;
use std::process::Command;
use std::time::Duration;
use tokio::time::sleep;

mod cli_commands;

#[cfg(feature = "tray")]
fn start_tray(port: u16) {
    // shortcut: real tray-icon menu needs a graphical session; feature-gated so
    // headless Linux CI builds without GTK. Upgrade: wire tray-icon menu callbacks.
    println!("🔔 Tray mode is not wired in this build (feature `tray` enabled but no menu callbacks). Server keeps running on port {port}.");
}

#[cfg(not(feature = "tray"))]
fn start_tray(port: u16) {
    println!("🔔 Tray icon is disabled in this build (build with --features tray). Server keeps running on port {port}.");
}

fn try_update(latest: &str) -> bool {
    match inquire::Confirm::new(&format!("Update now to v{latest}?"))
        .with_default(true)
        .prompt()
    {
        Ok(true) => {
            println!("   Updating via cargo…");
            let ok = std::process::Command::new("cargo")
                .args(["install", "--path", "crates/nr-cli"])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                println!("✅ Updated to v{latest}. Restart 9router to apply.");
            } else {
                println!("❌ cargo install failed.");
            }
            true // exit menu loop either way
        }
        _ => false,
    }
}

const DEFAULT_PORT: u16 = 20128;
const DEFAULT_HOST: &str = "0.0.0.0";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const PROCESS_EXIT_WAIT_MS: u64 = 1000;

#[derive(Parser, Debug)]
// clap's auto -h/--help and -V/--version would collide with (and disagree with)
// the JS flags: cli.js handles `-h|--help` itself and prints `-v|--version`.
#[command(
    name = "9router",
    about = "Local AI routing gateway",
    disable_help_flag = true,
    disable_version_flag = true
)]
struct Args {
    /// Port to listen on
    #[arg(short = 'p', long, env = "PORT", default_value_t = DEFAULT_PORT)]
    port: u16,

    /// Interface to bind
    #[arg(short = 'H', long = "host", visible_alias = "hostname", env = "HOSTNAME", default_value = DEFAULT_HOST)]
    hostname: String,

    /// Don't open browser automatically
    #[arg(short = 'n', long)]
    no_browser: bool,

    /// Show server logs
    #[arg(short = 'l', long)]
    log: bool,

    /// Skip update check
    #[arg(long)]
    skip_update: bool,

    /// Run in system tray mode (background)
    #[arg(short = 't', long)]
    tray: bool,

    /// Show help
    #[arg(short = 'h', long = "help")]
    help_flag: bool,

    /// Show version
    #[arg(short = 'v', long = "version")]
    version_flag: bool,

    /// Subcommand: connect <url>, show [tool...], xai video ...
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Parser, Debug)]
enum CliCommand {
    /// Configure CLI tools for a remote 9router server
    Connect {
        /// Server URL (e.g., http://localhost:20128)
        url: String,
        /// Additional connect arguments
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        extra: Vec<String>,
    },
    /// Show current CLI tool configuration
    Show {
        /// Specific tool names to show (optional)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        tools: Vec<String>,
    },
    /// Check GitHub releases for a newer version and offer to self-update
    Update {
        /// Additional update arguments
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        extra: Vec<String>,
    },
    /// Generate Grok Imagine videos via xAI
    #[command(name = "xai")]
    Xai {
        #[command(subcommand)]
        subcommand: XaiCommand,
    },
}

#[derive(Parser, Debug)]
enum XaiCommand {
    /// Generate a video
    Video {
        /// Additional video arguments
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    if args.help_flag {
        print_help();
        return Ok(());
    }

    if args.version_flag {
        println!("{VERSION}");
        return Ok(());
    }

    // Handle subcommands that don't need server
    if let Some(cmd) = args.command {
        return cli_commands::handle_command(cmd).await;
    }

    // Initialize logging
    let log_level = if args.log { "debug" } else { "info" };
    if env::var("RUST_LOG").is_err() {
        env::set_var("RUST_LOG", log_level);
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| log_level.into()))
        .init();

    // Main launcher flow: start the server
    run_launcher(args).await
}

/// Print CLI help text.
fn print_help() {
    println!("Usage: 9router [options]

Options:
  -p, --port <port>   Port to run the server (default: {})
  -H, --host <host>   Host to bind (default: {})
  -n, --no-browser    Don't open browser automatically
  -l, --log           Show server logs
  -t, --tray          Run in system tray mode (background)
  --skip-update       Skip auto-update check
  -h, --help          Show this help message
  -v, --version       Show version

Commands:
  connect <server-url> Configure Claude Code for a remote 9router server
  show [tool…]         Show each CLI tool's current 9router config
  update               Check GitHub releases and offer to self-update
  xai video …          Generate a Grok Imagine video via the running gateway

Examples:
  9router --port 20128
  9router --no-browser --tray
  npx 9router connect http://host:20128
  9router show claude
  9router xai video --prompt \"...\" --output video.mp4", DEFAULT_PORT, DEFAULT_HOST);
}

/// Run the main launcher: spawn server, present UI, handle lifecycle.
async fn run_launcher(args: Args) -> anyhow::Result<()> {
    let data_dir = paths::data_dir();

    // Ensure data directory exists
    fs::create_dir_all(&data_dir)
        .map_err(|e| anyhow::anyhow!("Failed to create data directory: {}", e))?;

    let pid_file = get_pid_file_path(&data_dir);
    let self_pid = std::process::id();

    // Stop any previous instance before starting a new one
    if let Ok(Some(old_record)) = read_pid_record(&pid_file) {
        let _killed = stop_recorded_instance(&old_record, self_pid);
        // Wait briefly for old process to exit
        sleep(Duration::from_millis(PROCESS_EXIT_WAIT_MS)).await;
    }
    let _ = fs::remove_file(&pid_file);

    // Write PID record for this launcher instance
    let record = PidRecord::new(self_pid, None, Some(args.port), "9router".to_string());
    let _ = write_pid_record(&pid_file, &record);

    // Show startup warning if network-exposed
    if args.hostname == DEFAULT_HOST {
        if let Some(ip) = get_lan_ip() {
            warn(format!(
                "Network-exposed: reachable at http://{}:{} (bound 0.0.0.0). Use --host 127.0.0.1 for local-only.",
                ip, args.port
            ));
        }
    }

    // Show version
    println!("🚀 9router v{}", VERSION);
    println!("Server: http://{}:{}", args.hostname, args.port);

    // Start version check in background (non-blocking)
    let skip_update = args.skip_update;
    let version_check = if !skip_update {
        tokio::spawn(async move { check_for_update().await })
    } else {
        tokio::spawn(async { None })
    };

    // Open browser if requested
    if !args.no_browser {
        let url = format!("http://localhost:{}/dashboard", args.port);
        let _ = open::that(&url);
    }

    // Wait for server to be ready
    println!("Starting server...");
    let port = args.port;
    let hostname = args.hostname.clone();

    // Run the embedded server with basic error handling
    let opts = nr_server::ServerOptions {
        port: args.port,
        hostname: args.hostname.clone(),
    };

    // Spawn server task
    let server_task = tokio::spawn(async move {
        match nr_server::run(opts).await {
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        }
    });

    // Wait for server to be ready, then show menu loop
    if wait_for_server_ready(port, Some(15000)).await {
        println!("✅ Server ready at http://{}:{}/dashboard\n", hostname, port);

        // Wait for version check result
        let latest_version = version_check.await.ok().flatten();

        // --tray runs the background/tray mode: no interactive menu, block until
        // the server is stopped so the dashboard keeps serving.
        if args.tray {
            start_tray(port);
            println!("   Dashboard: http://localhost:{port}/dashboard");
            println!("   Press Ctrl-C to stop the server.");
        } else {
            show_menu_loop(port, latest_version).await?;
        }
    } else {
        warn("Failed to start server");
    }

    // Clean up PID file on exit
    let _ = remove_pid_record_if_owner(&pid_file, self_pid);

    // Wait for server task to complete
    let _ = server_task.await;

    Ok(())
}

/// Show the interactive menu loop.
async fn show_menu_loop(port: u16, latest_version: Option<String>) -> anyhow::Result<()> {
    loop {
        clear_screen()?;
        println!("\n📡 9Router v{VERSION}\n");
        println!("   Server: http://localhost:{port}");
        println!("Choose interface:\n");

        let mut items: Vec<String> = Vec::new();
        if let Some(ref version) = latest_version {
            items.push(format!("⬆  Update to v{version} (current: v{VERSION})"));
        }
        let idx_browser = items.len();
        items.push("🌐 Web UI (Open in Browser)".into());
        let idx_tui = items.len();
        items.push("💻 Terminal UI (Interactive CLI)".into());
        let idx_tray = items.len();
        items.push("🔔 Hide to Tray (Background)".into());
        let idx_exit = items.len();
        items.push("🚪 Exit".into());

        let choice = if stdin_is_tty() {
            match inquire::Select::new("Select option", items.clone()).prompt() {
                Ok(sel) => items.iter().position(|i| *i == sel).unwrap_or(idx_exit),
                Err(_) => idx_exit, // Ctrl+C / Esc → exit
            }
        } else {
            // Non-interactive: print the menu and exit cleanly so piped runs terminate.
            for (i, item) in items.iter().enumerate() {
                println!("  {}. {item}", i + 1);
            }
            idx_exit
        };

        if choice == idx_browser {
            let url = format!("http://localhost:{port}/dashboard");
            let _ = open::that(&url);
            println!("\nOpening browser... Press Enter to return to menu.");
            pause();
        } else if choice == idx_tui {
            println!("\n💻 Terminal UI\n");
            println!("Interactive menu navigation is not wired in this build.");
            println!("Use the Web UI at http://localhost:{port}/dashboard");
            println!("\nPress Enter to return...");
            pause();
        } else if choice == idx_tray {
            start_tray(port);
            break;
        } else if latest_version.is_some() && choice == 0 {
            let version = latest_version.as_deref().unwrap_or("");
            if try_update(version) {
                break;
            }
        } else {
            println!("\n👋 Exiting...");
            break;
        }
    }
    Ok(())
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

fn pause() {
    let mut buf = String::new();
    let _ = std::io::stdin().read_line(&mut buf);
}

/// Check GitHub releases for a newer version (shared with the `update` command).
async fn check_for_update() -> Option<String> {
    cli_commands::check_for_update(VERSION).await
}

/// Get the first non-internal IPv4 address.
fn get_lan_ip() -> Option<String> {
    #[cfg(unix)]
    {
        // Try hostname -I first (Linux)
        if let Ok(output) = Command::new("hostname")
            .arg("-I")
            .output()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(ip) = stdout.split_whitespace().next() {
                return Some(ip.to_string());
            }
        }
        None
    }

    #[cfg(windows)]
    {
        // Try ipconfig on Windows
        if let Ok(output) = Command::new("ipconfig").output() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                if line.contains("IPv4 Address") {
                    if let Some(addr) = line.split(':').nth(1) {
                        return Some(addr.trim().to_string());
                    }
                }
            }
        }
        None
    }
}

