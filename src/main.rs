use std::path::PathBuf;

use anyhow::Context as _;
use clap::{Parser, Subcommand, ValueEnum};
use rmcp::ServiceExt as _;

use lani::daemon::{self, RunningServer};
use lani::mcp::LaniServer;
use lani::paths;
use lani::service::Service;
use lani::settings::{self, IdleTimeout};
use lani::web::{self, ServeOptions, View};

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
    /// Open the web UI in your browser, starting the server if needed.
    Open {
        /// Show this review.
        #[arg(long, conflicts_with = "issue")]
        review: Option<i64>,
        /// Show this issue.
        #[arg(long)]
        issue: Option<i64>,
        /// With --review: the view to show (default: overview, or files with --file).
        #[arg(long, value_enum, requires = "review")]
        view: Option<ViewArg>,
        /// With --review: a file or directory to show, optionally at a line: src/main.rs:42.
        #[arg(long, value_name = "PATH[:LINE]", requires = "review")]
        file: Option<String>,
        /// Print the sign-in URL to stdout. It contains the access token, so keep it private.
        #[arg(long)]
        print_url: bool,
        /// Do not open a browser.
        #[arg(long)]
        no_browser: bool,
    },
    /// Run the web server (in the foreground unless --detach is given).
    Serve {
        /// Port to listen on, on 127.0.0.1 (default: any free port).
        #[arg(long)]
        port: Option<u16>,
        /// Stop after this long without activity: e.g. 30m, 3h, 2d, or never.
        /// Overrides the saved setting for this run only.
        #[arg(long, value_name = "DURATION")]
        idle_timeout: Option<String>,
        /// Start the server in the background and return once it is ready.
        #[arg(long)]
        detach: bool,
    },
    /// Show whether the web server is running.
    Status,
    /// Stop the web server.
    Stop,
    /// Print the version.
    Version,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Mcp => run_mcp(),
        Command::Open {
            review,
            issue,
            view,
            file,
            print_url,
            no_browser,
        } => {
            let path = match (review, issue) {
                (_, Some(issue)) => format!("/issues/{issue}"),
                (Some(review), None) => review_path(review, view, file.as_deref())?,
                (None, None) => "/".to_string(),
            };
            open(&path, print_url, no_browser)
        }
        Command::Serve {
            port,
            idle_timeout,
            detach,
        } => serve(port, idle_timeout.as_deref(), detach),
        Command::Status => status(),
        Command::Stop => stop(),
        Command::Version => {
            println!("lani {}", daemon::VERSION);
            Ok(())
        }
    }
}

/// Log to stderr; `LANI_LOG` sets the filter (e.g. "debug"). In `lani mcp`, stdout carries the
/// protocol; a detached server's stderr goes to its log file.
fn init_logging() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("LANI_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
}

fn home() -> anyhow::Result<PathBuf> {
    let home = paths::lani_home()?;
    paths::ensure_home(&home)?;
    Ok(home)
}

fn run_mcp() -> anyhow::Result<()> {
    init_logging();
    let home = home()?;
    let db_path = paths::db_path(&home);
    let service = Service::open(&db_path)
        .with_context(|| format!("opening the LANI database at {}", db_path.display()))?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let running = LaniServer::new(service, home)
            .serve(rmcp::transport::stdio())
            .await
            .context("starting the MCP server")?;
        running.waiting().await.context("running the MCP server")?;
        anyhow::Ok(())
    })
}

/// The views `lani open --view` accepts.
#[derive(Clone, Copy, ValueEnum)]
enum ViewArg {
    Overview,
    Changes,
    Files,
}

/// The page path for `lani open --review` with an optional view and `PATH[:LINE]`. A trailing
/// `:<digits>` is a line number; any other colon is part of the path.
fn review_path(review: i64, view: Option<ViewArg>, file: Option<&str>) -> anyhow::Result<String> {
    let (path, line) = match file.and_then(|file| file.rsplit_once(':')) {
        Some((path, line)) if !line.is_empty() && line.bytes().all(|b| b.is_ascii_digit()) => (
            Some(path),
            Some(
                line.parse::<u32>()
                    .context("the line number is too large")?,
            ),
        ),
        _ => (file, None),
    };
    let view = match (view, path) {
        (Some(ViewArg::Overview), _) | (None, None) => View::Overview,
        (Some(ViewArg::Changes), _) => View::Changes,
        (Some(ViewArg::Files), _) | (None, Some(_)) => View::Files,
    };
    Ok(web::review_page_path(review, view, path, line)?)
}

fn open(path: &str, print_url: bool, no_browser: bool) -> anyhow::Result<()> {
    let home = home()?;
    let path = path.to_string();
    let (server, started) = daemon::ensure_running(&home)?;
    warn_on_version_mismatch(&server);
    if started {
        eprintln!("Started the LANI server (pid {}).", server.health.pid);
    }
    if print_url {
        println!("{}", server.file.login_url(&path));
    }
    if !no_browser {
        daemon::open_in_browser(&home, &server.file, &path).with_context(|| {
            "could not open a browser; run `lani open --print-url` and open the printed URL yourself"
        })?;
        eprintln!("Opened {}", server.file.public_url(&path));
    }
    Ok(())
}

fn serve(port: Option<u16>, idle_timeout: Option<&str>, detach: bool) -> anyhow::Result<()> {
    let home = home()?;
    let idle_timeout = idle_timeout.map(IdleTimeout::parse).transpose()?;
    if detach {
        if let Some(server) = daemon::find_running(&home) {
            eprintln!(
                "A LANI server is already running (pid {}, port {}); its settings were not changed.",
                server.health.pid, server.health.port
            );
            return Ok(());
        }
        let timeout_text = idle_timeout.map(|timeout| timeout.to_string());
        let server = daemon::start_detached(&home, port, timeout_text.as_deref())?;
        eprintln!(
            "LANI server running on 127.0.0.1:{} (pid {}).",
            server.health.port, server.health.pid
        );
        return Ok(());
    }
    init_logging();
    web::serve(&ServeOptions {
        home,
        port,
        idle_timeout,
    })?;
    Ok(())
}

fn status() -> anyhow::Result<()> {
    let home = home()?;
    let Some(server) = daemon::find_running(&home) else {
        println!("No LANI server is running.");
        return Ok(());
    };
    let health = &server.health;
    println!(
        "LANI server running: pid {}, port {}, version {}, started {}",
        health.pid,
        health.port,
        health.version,
        lani::time::to_local(&health.started_at)
    );
    let remaining = health.idle_remaining_secs.map_or_else(
        || "it never stops on its own".to_string(),
        |secs| {
            format!(
                "it stops after {} without activity",
                settings::hours_minutes(std::time::Duration::from_secs(secs))
            )
        },
    );
    println!(
        "Idle timeout {} ({}); {remaining}.",
        health.idle_timeout, health.idle_timeout_source
    );
    warn_on_version_mismatch(&server);
    Ok(())
}

fn stop() -> anyhow::Result<()> {
    let home = home()?;
    match daemon::stop(&home)? {
        Some(server) => println!("Stopped the LANI server (pid {}).", server.health.pid),
        None => println!("No LANI server is running."),
    }
    Ok(())
}

fn warn_on_version_mismatch(server: &RunningServer) {
    if let Some(warning) = server.version_warning() {
        eprintln!("Warning: {warning}");
    }
}
