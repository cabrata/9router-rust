//! CLI command handlers for subcommands like `connect`, `show`, `xai video`.

use crate::{CliCommand, XaiCommand};
use anyhow::Result;

/// Handle subcommands that don't require the server.
pub async fn handle_command(cmd: CliCommand) -> Result<()> {
    match cmd {
        CliCommand::Connect { url } => handle_connect(&url).await,
        CliCommand::Show { tools } => handle_show(&tools).await,
        CliCommand::Xai { subcommand } => handle_xai(subcommand).await,
    }
}

async fn handle_connect(url: &str) -> Result<()> {
    println!("Configuring CLI tools for remote server: {}", url);
    // TODO: Port connect functionality from cli/src/cli/commands/connect.js
    Ok(())
}

async fn handle_show(tools: &[String]) -> Result<()> {
    println!("Current configuration:");
    if tools.is_empty() {
        println!("  Show all tools");
    } else {
        for tool in tools {
            println!("  - {}", tool);
        }
    }
    // TODO: Port show functionality from cli/src/cli/commands/show.js
    Ok(())
}

async fn handle_xai(cmd: XaiCommand) -> Result<()> {
    match cmd {
        XaiCommand::Video { prompt, output } => {
            println!("Generating video with prompt: {}", prompt);
            println!("Output: {}", output);
            // TODO: Port xai video functionality from cli/src/cli/commands/xaiVideo.js
            Ok(())
        }
    }
}
