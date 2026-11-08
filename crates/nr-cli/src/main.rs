//! The `9router` command. Port of cli/ (owner: cli agent, see AGENTS.md).
//! Wave-0 skeleton: just runs the server.

use clap::Parser;

#[derive(Parser)]
#[command(name = "9router", version, about = "Local AI routing gateway")]
struct Args {
    /// Port to listen on.
    #[arg(long, env = "PORT", default_value_t = 20128)]
    port: u16,
    /// Interface to bind.
    #[arg(long, env = "HOSTNAME", default_value = "0.0.0.0")]
    hostname: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    let args = Args::parse();
    nr_server::run(nr_server::ServerOptions { port: args.port, hostname: args.hostname }).await
}
