//! The `9router` command. Port of cli/ (owner: cli agent, see AGENTS.md).
//!
//! Manages the lifecycle of the 9router server: spawning it, tracking via PID file,
//! presenting interactive menus, and handling graceful shutdown.

use clap::Parser;
use nr_cli::launcher_process::{get_pid_file_path, read_pid_record, write_pid_record, PidRecord};
use nr_cli::process_management::stop_recorded_instance;
use nr_core::paths;
use std::env;

mod cli_commands;

const DEFAULT_PORT: u16 = 20128;
const DEFAULT_HOST: &str = "0.0.0.0";
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Debug)]
#[command(name = "9router", version = VERSION, about = "Local AI routing gateway")]
struct Args {
    /// Port to listen on
    #[arg(long, env = "PORT", default_value_t = DEFAULT_PORT)]
    port: u16,

    /// Interface to bind
    #[arg(long, env = "HOSTNAME", default_value = DEFAULT_HOST)]
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
    },
    /// Show current CLI tool configuration
    Show {
        /// Specific tool names to show (optional)
        tools: Vec<String>,
    },
    /// Generate a Grok Imagine video
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
        #[arg(long)]
        prompt: String,
        #[arg(long)]
        output: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();

    // Handle subcommands that don't need server
    if let Some(cmd) = args.command {
        return cli_commands::handle_command(cmd).await;
    }

    // Main launcher flow: start the server
    run_launcher(args).await
}

/// Run the main launcher: spawn server, present UI, handle lifecycle.
async fn run_launcher(args: Args) -> anyhow::Result<()> {
    let data_dir = paths::data_dir();
    let pid_file = get_pid_file_path(data_dir);
    let self_pid = std::process::id();

    // Stop any previous instance before starting a new one
    if let Ok(Some(old_record)) = read_pid_record(&pid_file) {
        let _killed = stop_recorded_instance(&old_record, self_pid);
    }
    let _ = std::fs::remove_file(&pid_file);

    // Write PID record for this launcher instance
    let record = PidRecord::new(self_pid, Some(self_pid), Some(args.port), "9router".to_string());
    let _ = write_pid_record(&pid_file, &record);

    // Show startup message
    if args.hostname == DEFAULT_HOST {
        let lan_ip = get_lan_ip();
        if let Some(ip) = lan_ip {
            println!(
                "\x1b[33m⚠️  Network-exposed: reachable at http://{}:{} (bound 0.0.0.0). Use --host 127.0.0.1 for local-only.\x1b[0m",
                ip, args.port
            );
        }
    }

    println!("🚀 9router v{}", VERSION);
    println!("Server: http://{}:{}", args.hostname, args.port);

    // Open browser if requested
    if !args.no_browser && args.hostname == DEFAULT_HOST {
        let url = format!("http://localhost:{}/dashboard", args.port);
        let _ = open::that(&url);
    }

    // Run the server (blocks until shutdown)
    let opts = nr_server::ServerOptions {
        port: args.port,
        hostname: args.hostname,
    };

    nr_server::run(opts).await?;

    // Clean up PID file when server exits
    let _ = nr_cli::launcher_process::remove_pid_record_if_owner(&pid_file, self_pid);

    Ok(())
}

/// Get the first non-internal IPv4 address.
fn get_lan_ip() -> Option<String> {
    #[cfg(unix)]
    {
        use std::process::Command;
        // Try to get non-loopback IPv4 via hostname -I (Linux) or ifconfig (macOS)
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
        use std::process::Command;
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

