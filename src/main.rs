use std::path::PathBuf;

use anyhow::Context as _;
use clap::{Parser, Subcommand, ValueEnum};
use rmcp::ServiceExt as _;

use lani::daemon::{self, RunningServer};
use lani::mcp::LaniServer;
use lani::mcp::render;
use lani::model::Actor;
use lani::paths;
use lani::port::PortChoice;
use lani::service::{Service, WaitOutcome};
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
        /// Listen on exactly this port, on 127.0.0.1; 0 picks any free port. Without it, the
        /// server takes the first free port from the configured one (default 5264).
        #[arg(long)]
        port: Option<u16>,
        /// Stop after this long without activity: e.g. 30m, 3h, 2d, or never.
        /// Overrides the saved setting for this run only.
        #[arg(long, value_name = "DURATION")]
        idle_timeout: Option<String>,
        /// Start the server in the background and return once it is ready. Either way,
        /// `lani serve` prints the sign-in link to stdout.
        #[arg(long)]
        detach: bool,
    },
    /// Show whether the web server is running.
    Status {
        /// Print only the running server's sign-in URL to stdout, never starting a server; exit 1
        /// if none is running. The URL contains the access token, so keep it private.
        #[arg(long)]
        print_url: bool,
    },
    /// Stop the web server.
    Stop,
    /// Wait until it is a role's turn on a review, then print what to do (§11.5). Claude runs this
    /// as a background command after `hand_off`. Exits 0 when it is the role's turn, 3 if the
    /// review is closed, and 4 when --timeout passes; 1 and 2 are errors, as for every command.
    Wait {
        /// The review to watch.
        #[arg(long)]
        review: i64,
        /// The role that is waiting.
        #[arg(long = "for", value_enum, value_name = "ROLE")]
        role: RoleArg,
        /// Give up after this long: a whole number of seconds, minutes or hours, like 30s, 10m or
        /// 2h. Without it, wait until the turn comes or the review is closed.
        #[arg(long, value_name = "DURATION")]
        timeout: Option<String>,
    },
    /// Print the version.
    Version,
}

/// The roles that wait for their turn.
#[derive(Clone, Copy, ValueEnum)]
enum RoleArg {
    Reviewer,
    Implementer,
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
        Command::Status { print_url } => status(print_url),
        Command::Stop => stop(),
        Command::Wait {
            review,
            role,
            timeout,
        } => wait(review, role, timeout.as_deref()),
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

/// How often `lani wait` checks the review.
const WAIT_POLL: std::time::Duration = std::time::Duration::from_secs(1);

fn wait(review_id: i64, role: RoleArg, timeout: Option<&str>) -> anyhow::Result<()> {
    let role = match role {
        RoleArg::Reviewer => Actor::Reviewer,
        RoleArg::Implementer => Actor::Implementer,
    };
    let timeout = timeout.map(parse_wait_timeout).transpose()?;
    let service = Service::open(&paths::db_path(&home()?))?;
    // The output reaches Claude like a tool result, so it follows the same contract (§12.4).
    let print = |text: &str| print!("{}", lani::text::neutralize_line_breaks(text));
    match service.wait_for_turn(review_id, role, timeout, WAIT_POLL)? {
        WaitOutcome::YourTurn(view) => {
            print(&render::your_turn(&view, role));
            Ok(())
        }
        WaitOutcome::Closed(_) => {
            print(&format!(
                "Review {review_id} was closed; there is nothing more to wait for.\n"
            ));
            std::process::exit(3);
        }
        WaitOutcome::TimedOut(review) => {
            let holder = match review.turn {
                Some(holder) => format!("the {}", holder.as_str()),
                None => "nobody".to_string(),
            };
            print(&format!(
                "Still not your turn on review {review_id}: it is {holder}'s turn.\n"
            ));
            // Not 2, which clap uses for usage errors.
            std::process::exit(4);
        }
    }
}

/// A `lani wait --timeout`: a whole number of seconds, minutes or hours.
fn parse_wait_timeout(text: &str) -> anyhow::Result<std::time::Duration> {
    let invalid = || {
        anyhow::anyhow!(
            "invalid timeout {text:?}: use a whole number followed by s, m or h, like 30s, 10m or 2h"
        )
    };
    let unit_at = text
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(invalid)?;
    let (number, unit) = text.split_at(unit_at);
    let number: u64 = number.parse().map_err(|_| invalid())?;
    let seconds = match unit {
        "s" => number,
        "m" => number.checked_mul(60).ok_or_else(invalid)?,
        "h" => number.checked_mul(3600).ok_or_else(invalid)?,
        _ => return Err(invalid()),
    };
    Ok(std::time::Duration::from_secs(seconds))
}

fn home() -> anyhow::Result<PathBuf> {
    let home = paths::lani_home()?;
    paths::ensure_home(&home)?;
    Ok(home)
}

fn run_mcp() -> anyhow::Result<()> {
    init_logging();
    // Taken first, so that any reinstall after this process started is noticed (§12.5).
    let executable = lani::executable::Executable::current()
        .inspect_err(|error| {
            tracing::warn!(
                "cannot find this lani executable, so a reinstall goes unnoticed: {error}"
            );
        })
        .ok();
    let home = home()?;
    let db_path = paths::db_path(&home);
    let service = Service::open(&db_path)
        .with_context(|| format!("opening the LANI database at {}", db_path.display()))?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let running = LaniServer::new(service, home, executable)
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
    // The flag is checked at once. The configuration file is read only when a server is started,
    // by `start_detached` or by the server itself, so a mistake in it doesn't stop `lani serve`
    // naming a server that is already running.
    let flag = port.map(PortChoice::flag).transpose()?;
    if detach {
        let server = if let Some(server) = daemon::find_running(&home) {
            eprintln!(
                "A LANI server is already running (pid {}, port {}); its settings were not changed.",
                server.health.pid, server.health.port
            );
            server
        } else {
            let timeout_text = idle_timeout.map(|timeout| timeout.to_string());
            let server = daemon::start_detached(&home, port, timeout_text.as_deref())?;
            eprintln!(
                "LANI server running on 127.0.0.1:{} (pid {}).",
                server.health.port, server.health.pid
            );
            server
        };
        print!("{}", server.file.sign_in_text());
        return Ok(());
    }
    init_logging();
    web::serve(&ServeOptions {
        home,
        port: flag,
        idle_timeout,
    })?;
    Ok(())
}

fn status(print_url: bool) -> anyhow::Result<()> {
    let home = home()?;
    let Some(server) = daemon::find_running(&home) else {
        if print_url {
            anyhow::bail!(
                "No LANI server is running, so there is no sign-in URL. Start one with \
                 `lani serve --detach` or `lani open`."
            );
        }
        println!("No LANI server is running.");
        return Ok(());
    };
    if print_url {
        println!("{}", server.file.login_url("/"));
        warn_on_version_mismatch(&server);
        return Ok(());
    }
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
    let Some(stopped) = daemon::stop(&home)? else {
        println!("No LANI server is running.");
        return Ok(());
    };
    println!(
        "Stopped the LANI server (pid {}).",
        stopped.server.health.pid
    );
    if let Some(successor) = stopped.successor {
        println!(
            "Meanwhile another command started a new LANI server (pid {}, port {}); it is still \
             running.",
            successor.health.pid, successor.health.port
        );
    }
    Ok(())
}

fn warn_on_version_mismatch(server: &RunningServer) {
    if let Some(warning) = server.version_warning() {
        eprintln!("Warning: {warning}");
    }
}
