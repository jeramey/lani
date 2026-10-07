use anyhow::Context as _;
use clap::{Parser, Subcommand};
use rmcp::ServiceExt as _;

use lani::mcp::LaniServer;
use lani::paths;
use lani::service::Service;

/// LANI — Local Annotations, No Internet: a local code-review loop for Claude Code.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the MCP server on stdin/stdout (Claude Code starts this).
    Mcp,
    /// Print the version.
    Version,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Mcp => run_mcp(),
        Command::Version => {
            println!("lani {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

fn run_mcp() -> anyhow::Result<()> {
    // stdout carries the MCP protocol, so logs go to stderr. LANI_LOG sets the filter (e.g. "debug").
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("LANI_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let home = paths::lani_home()?;
    paths::ensure_home(&home)?;
    let db_path = paths::db_path(&home);
    let service = Service::open(&db_path)
        .with_context(|| format!("opening the LANI database at {}", db_path.display()))?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let running = LaniServer::new(service)
            .serve(rmcp::transport::stdio())
            .await
            .context("starting the MCP server")?;
        running.waiting().await.context("running the MCP server")?;
        anyhow::Ok(())
    })
}
