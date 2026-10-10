//! The local web UI server (docs/design.md §13).
//!
//! Pages are rendered on the server and every action is a plain form POST that redirects back
//! (or re-renders with an error). One small script, `static/lani.js`, adds line selection and
//! the new-issue form to the code views, and keeps review pages up to date (§13.5).

mod actions;
mod activity;
mod auth;
mod code;
mod live;
mod pages;
mod views;

use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::{get, post};
use tokio::sync::watch;

use crate::daemon::{self, Endpoint, ServerFile};
use crate::error::{Error, Result};
use crate::executable::Executable;
use crate::listen::{Binding, Listen};
use crate::port::PortChoice;
use crate::service::Service;
use crate::settings::IdleTimeout;

pub use auth::{browser_port, is_safe_next, same_origin};
pub use views::{View, repo_location, review_page_path};

/// How often the background task checks the idle timer and sweeps redirect files.
const HOUSEKEEPING_INTERVAL: Duration = Duration::from_secs(15);

/// Where the effective idle timeout came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeoutSource {
    Saved,
    Flag,
}

impl TimeoutSource {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Saved => "saved setting",
            Self::Flag => "--idle-timeout flag",
        }
    }
}

/// The idle timer (§13.1): any authenticated request except health checks counts as activity.
#[derive(Clone, Copy, Debug)]
pub struct IdleState {
    pub timeout: IdleTimeout,
    pub source: TimeoutSource,
    pub last_activity: Instant,
}

impl IdleState {
    /// Time left before the server should stop; `None` when the timeout is `never`.
    #[must_use]
    pub fn remaining(&self, now: Instant) -> Option<Duration> {
        match self.timeout {
            IdleTimeout::Never => None,
            IdleTimeout::After(limit) => {
                Some(limit.saturating_sub(now.saturating_duration_since(self.last_activity)))
            }
        }
    }

    #[must_use]
    pub fn expired(&self, now: Instant) -> bool {
        self.remaining(now) == Some(Duration::ZERO)
    }
}

/// Why the server is stopping, for the notice open pages show (§13.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The idle timeout passed.
    Idle(IdleTimeout),
    /// `lani stop`, or a signal.
    Requested,
}

impl Stop {
    /// The notice for open pages, ending in `reopen`, how to get LANI back
    /// ([`Endpoint::open_hint`]).
    #[must_use]
    pub fn message(self, reopen: &str) -> String {
        match self {
            Self::Idle(timeout) => format!("LANI stopped after {timeout} idle. {reopen}"),
            Self::Requested => format!("LANI was stopped. {reopen}"),
        }
    }
}

/// Fixed facts about this server instance.
#[derive(Clone, Debug)]
pub struct Instance {
    pub token: String,
    pub instance_id: String,
    pub endpoint: Endpoint,
    pub pid: u32,
    pub started_at: String,
    pub home: PathBuf,
}

/// State shared by all request handlers.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    service: Mutex<Service>,
    instance: Instance,
    /// The executable this server started from; `None` if it could not be found, in which case
    /// pages never show the update banner (§13.5).
    executable: Option<Executable>,
    idle: Mutex<IdleState>,
    shutdown: watch::Sender<Option<Stop>>,
}

impl AppState {
    #[must_use]
    pub fn new(
        service: Service,
        instance: Instance,
        executable: Option<Executable>,
        (timeout, source): (IdleTimeout, TimeoutSource),
    ) -> Self {
        let (shutdown, _) = watch::channel(None);
        Self {
            inner: Arc::new(Inner {
                service: Mutex::new(service),
                instance,
                executable,
                idle: Mutex::new(IdleState {
                    timeout,
                    source,
                    last_activity: Instant::now(),
                }),
                shutdown,
            }),
        }
    }

    /// Whether a newer lani has been installed over the executable this server started from, so
    /// pages show the update banner (§13.5).
    #[must_use]
    pub fn outdated(&self) -> bool {
        self.inner
            .executable
            .as_ref()
            .is_some_and(Executable::replaced)
    }

    #[must_use]
    pub fn instance(&self) -> &Instance {
        &self.inner.instance
    }

    /// What a review page rendered now needs for live updates, given the latest event it reflects.
    #[must_use]
    pub fn live(&self, events_after: i64) -> views::Live {
        let endpoint = &self.instance().endpoint;
        views::Live {
            events_after,
            outdated: self.outdated(),
            reopen: endpoint.open_hint(),
            restart: endpoint.restart_hint(),
        }
    }

    /// Run a service operation on the blocking thread pool; SQLite and git calls block. A database
    /// that a newer lani has upgraded comes back as [`Error::Outdated`] with the whole message the
    /// web UI shows, including how to restart this server (§7.4).
    pub async fn call<T, F>(&self, operation: F) -> Result<T>
    where
        F: FnOnce(&mut Service) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let mut service = inner.service.lock().unwrap_or_else(PoisonError::into_inner);
            service.check_schema()?;
            operation(&mut service)
        })
        .await
        .map_err(|error| Error::Internal(format!("a request handler failed: {error}")))?
        .map_err(|error| match error {
            Error::Outdated(message) => Error::Outdated(views::outdated_server_message(
                &message,
                &self.instance().endpoint.restart_hint(),
            )),
            other => other,
        })
    }

    #[must_use]
    pub fn idle(&self) -> IdleState {
        *self
            .inner
            .idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub fn record_activity(&self) {
        self.inner
            .idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last_activity = Instant::now();
    }

    pub fn set_idle_timeout(&self, timeout: IdleTimeout, source: TimeoutSource) {
        let mut idle = self
            .inner
            .idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        idle.timeout = timeout;
        idle.source = source;
    }

    /// Stop the server; the first reason given is the one open pages are told.
    pub fn request_shutdown(&self, stop: Stop) {
        self.inner.shutdown.send_if_modified(|current| {
            let first = current.is_none();
            if first {
                *current = Some(stop);
            }
            first
        });
    }

    /// Resolves with the reason once a shutdown has been requested.
    pub async fn shutdown_requested(&self) -> Stop {
        let mut receiver = self.inner.shutdown.subscribe();
        let Ok(stop) = receiver.wait_for(Option::is_some).await else {
            // The sender lives as long as this state, so it cannot have been dropped.
            return Stop::Requested;
        };
        stop.unwrap_or(Stop::Requested)
    }
}

/// All routes, behind the authentication and security-header middleware.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(pages::index))
        .route("/reviews/{review_id}", get(pages::review))
        .route("/reviews/{review_id}/issues/{issue_id}", get(pages::issue))
        .route("/issues/{issue_id}", get(pages::issue_redirect))
        .route("/reviews/{review_id}/changes", get(code::changes))
        .route("/reviews/{review_id}/events", get(live::events))
        .route("/reviews/{review_id}/files", get(code::files_bare))
        .route("/reviews/{review_id}/files/", get(code::files_root))
        .route("/reviews/{review_id}/files/{*path}", get(code::files))
        .route(
            "/settings",
            get(pages::settings).post(actions::save_settings),
        )
        .route("/static/lani.css", get(pages::stylesheet))
        .route("/static/highlight.css", get(pages::highlight_stylesheet))
        .route("/static/lani.js", get(pages::script))
        .route("/login", get(auth::login))
        .route("/api/health", get(pages::health))
        .route("/api/shutdown", post(actions::shutdown))
        .route("/reviews/{review_id}/title", post(actions::retitle))
        .route("/reviews/{review_id}/base", post(actions::change_base))
        .route("/reviews/{review_id}/summary", post(actions::edit_summary))
        .route(
            "/reviews/{review_id}/comments",
            post(actions::comment_on_summary),
        )
        .route("/reviews/{review_id}/close", post(actions::close_review))
        .route("/reviews/{review_id}/turn", post(actions::hand_off))
        .route("/reviews/{review_id}/prompts", post(actions::save_prompt))
        .route("/settings/round-limit", post(actions::save_round_limit))
        .route("/reviews/{review_id}/reopen", post(actions::reopen_review))
        .route(
            "/reviews/{review_id}/issues/{issue_id}/comments",
            post(actions::comment_on_issue),
        )
        .route(
            "/reviews/{review_id}/issues/{issue_id}/status",
            post(actions::set_issue_status),
        )
        .route(
            "/reviews/{review_id}/issues/{issue_id}/details",
            post(actions::edit_issue),
        )
        .route("/comments/{comment_id}/edit", post(actions::edit_comment))
        .route("/reviews/{review_id}/issues", post(actions::create_issue))
        .route("/preview", post(actions::preview))
        .fallback(pages::not_found)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::guard,
        ))
        .with_state(state)
}

/// Options for `lani serve`.
pub struct ServeOptions {
    pub home: PathBuf,
    /// The port from `--port`. For what the flags leave open, the server reads the configuration
    /// file once it holds the server lock, so a mistake in the file doesn't stop it naming a
    /// running server.
    pub port: Option<PortChoice>,
    /// `--listen`: a TCP port or a Unix socket (§13.2).
    pub listen: Option<Listen>,
    /// Overrides the saved idle timeout for this run.
    pub idle_timeout: Option<IdleTimeout>,
}

/// Run the web server until it is stopped. Exits quietly if another server is already running.
pub fn serve(options: &ServeOptions) -> Result<()> {
    // Taken first, so that any reinstall after this server started is noticed (§13.5).
    let executable = Executable::current()
        .inspect_err(|error| {
            eprintln!("cannot find this lani executable, so a reinstall goes unnoticed: {error}");
        })
        .ok();
    daemon::ensure_dirs(&options.home)?;
    let Some(_lock) = daemon::ServerLock::try_acquire(&options.home)? else {
        match daemon::find_running(&options.home) {
            Some(server) => {
                eprintln!(
                    "A LANI server is already running (pid {}, {}); this one was not started.",
                    server.health.pid,
                    server.health.endpoint.describe()
                );
                print!("{}", server.file.sign_in_text());
            }
            // Still starting, or stopping: it holds the lock but doesn't answer yet, or anymore.
            None => eprintln!("A LANI server is already running; use `lani open` to reach it."),
        }
        return Ok(());
    };
    let binding = Binding::choose(&options.home, options.port, options.listen)?;
    let service = Service::open(&crate::paths::db_path(&options.home))?;
    let (timeout, source) = match options.idle_timeout {
        Some(timeout) => (timeout, TimeoutSource::Flag),
        None => (service.idle_timeout()?, TimeoutSource::Saved),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    runtime.block_on(run(
        options,
        binding,
        service,
        executable,
        (timeout, source),
    ))
}

/// A listening socket of either kind.
enum Listener {
    Tcp(tokio::net::TcpListener),
    Socket(tokio::net::UnixListener),
}

async fn run(
    options: &ServeOptions,
    binding: Binding,
    service: Service,
    executable: Option<Executable>,
    (timeout, source): (IdleTimeout, TimeoutSource),
) -> Result<()> {
    let (listener, endpoint, taken) = match binding {
        Binding::Tcp(choice) => {
            let listener = listen(choice).await?;
            let port = listener.local_addr()?.port();
            let taken = match choice {
                PortChoice::From(base) if base.get() != port => {
                    format!("; port {base} was in use")
                }
                _ => String::new(),
            };
            (Listener::Tcp(listener), Endpoint::Tcp { port }, taken)
        }
        Binding::Socket { path, browser_port } => (
            Listener::Socket(listen_on_socket(&path)?),
            Endpoint::Socket {
                socket: path,
                browser_port: browser_port.get(),
                ssh_host: daemon::host_name(),
            },
            String::new(),
        ),
    };
    let instance = Instance {
        token: daemon::random_hex(32)?,
        instance_id: daemon::random_hex(16)?,
        endpoint: endpoint.clone(),
        pid: std::process::id(),
        started_at: crate::time::now(),
        home: options.home.clone(),
    };
    let server_file = ServerFile {
        pid: instance.pid,
        endpoint,
        token: instance.token.clone(),
        instance_id: instance.instance_id.clone(),
        version: daemon::VERSION.to_string(),
        started_at: instance.started_at.clone(),
    };
    let state = AppState::new(service, instance, executable, (timeout, source));
    server_file.write(&options.home)?;
    let address = match &server_file.endpoint {
        Endpoint::Tcp { port } => format!("127.0.0.1:{port}"),
        Endpoint::Socket { socket, .. } => socket.display().to_string(),
    };
    eprintln!(
        "LANI server listening on {address} (pid {}){taken}",
        server_file.pid
    );
    // The sign-in link holds the token, so it goes to stdout, never to the log on stderr. A
    // detached server's stdout is /dev/null (§13.2).
    print!("{}", server_file.sign_in_text());

    tokio::spawn(housekeeping(state.clone()));
    tokio::spawn(handle_signals(state.clone()));

    let served = match listener {
        Listener::Tcp(listener) => serve_until_stopped(listener, state.clone()).await,
        Listener::Socket(listener) => serve_until_stopped(listener, state.clone()).await,
    };
    // `lani stop` waits for the server lock, which this process holds until it exits, so this
    // cleanup is done by the time `lani stop` returns.
    daemon::remove_open_files(&options.home, Duration::ZERO);
    // Best effort, like the redirect files, so that server.json is still removed: a socket left
    // behind is replaced at the next start.
    if let Err(error) = daemon::remove_socket(&options.home) {
        eprintln!("could not remove the socket: {error}");
    }
    ServerFile::remove_if_ours(&options.home, &server_file.instance_id)?;
    eprintln!("LANI server stopped");
    served.map_err(Error::Io)
}

/// Serve the web UI on `listener` until a shutdown is requested.
async fn serve_until_stopped<L>(listener: L, state: AppState) -> std::io::Result<()>
where
    L: axum::serve::Listener,
    L::Addr: std::fmt::Debug,
{
    axum::serve(listener, router(state.clone()))
        .with_graceful_shutdown(async move {
            state.shutdown_requested().await;
        })
        .await
}

/// Listen on the Unix socket at `path`, with mode 0600 like `server.json`, in the 0700 run
/// directory. The caller holds the server lock, so a socket already there was left by a server
/// that died, and it is replaced (§13.1).
fn listen_on_socket(path: &Path) -> Result<tokio::net::UnixListener> {
    let cannot = |error: std::io::Error| {
        Error::Unavailable(format!("cannot listen on {} ({error})", path.display()))
    };
    // Anything other than a socket stays, and binding then fails with an error naming the path.
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket()) {
        std::fs::remove_file(path).map_err(cannot)?;
    }
    let listener = tokio::net::UnixListener::bind(path).map_err(cannot)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(cannot)?;
    Ok(listener)
}

/// Listen on 127.0.0.1 at the chosen port. A port in use is skipped only when counting up from a
/// base port; a port given with `--port` must be free.
async fn listen(choice: PortChoice) -> Result<tokio::net::TcpListener> {
    let candidates = choice.candidates();
    for &port in &candidates {
        match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => return Ok(listener),
            Err(error)
                if error.kind() == std::io::ErrorKind::AddrInUse
                    && matches!(choice, PortChoice::From(_)) => {}
            Err(error) => return Err(cannot_listen(choice, port, &error)),
        }
    }
    let (first, last) = (candidates.first(), candidates.last());
    Err(Error::Unavailable(format!(
        "ports {} to {} on 127.0.0.1 are all in use; {}",
        first.copied().unwrap_or_default(),
        last.copied().unwrap_or_default(),
        change_port(choice)
    )))
}

/// Why the server cannot listen on `port`, and what to change.
fn cannot_listen(choice: PortChoice, port: u16, error: &std::io::Error) -> Error {
    let problem = if error.kind() == std::io::ErrorKind::AddrInUse {
        format!("port {port} on 127.0.0.1 is already in use")
    } else {
        format!("cannot listen on port {port} on 127.0.0.1 ({error})")
    };
    Error::Unavailable(format!("{problem}; {}", change_port(choice)))
}

/// What to change to use another port: the flag that chose this one, or the configuration file.
fn change_port(choice: PortChoice) -> String {
    match choice {
        PortChoice::Exact(_) => "pass another --port, or leave --port out to use the first free \
                                 port from the configured one"
            .to_string(),
        PortChoice::From(_) | PortChoice::Any => format!(
            "set another port in {}, or pass --port",
            crate::paths::shown_config_path()
        ),
    }
}

/// Stop when idle, and delete expired browser-launch redirect files.
async fn housekeeping(state: AppState) {
    let mut ticker = tokio::time::interval(HOUSEKEEPING_INTERVAL);
    loop {
        ticker.tick().await;
        daemon::sweep_open_files(&state.instance().home);
        let idle = state.idle();
        if idle.expired(Instant::now()) {
            eprintln!("LANI server idle for {}; stopping", idle.timeout);
            state.request_shutdown(Stop::Idle(idle.timeout));
            return;
        }
    }
}

/// SIGTERM and SIGINT stop the server; SIGHUP is ignored so closing the terminal that started
/// the server does not stop it.
async fn handle_signals(state: AppState) {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut terminate), Ok(mut interrupt), Ok(mut hangup)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
        signal(SignalKind::hangup()),
    ) else {
        eprintln!("could not install signal handlers");
        return;
    };
    loop {
        tokio::select! {
            _ = terminate.recv() => break,
            _ = interrupt.recv() => break,
            _ = hangup.recv() => {}
        }
    }
    state.request_shutdown(Stop::Requested);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_port_that_cannot_be_used_is_named_with_what_to_change() {
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let message = |choice| cannot_listen(choice, 1000, &denied).to_string();
        let port = std::num::NonZeroU16::new(1000).unwrap();
        let flag = message(PortChoice::Exact(port));
        assert!(
            flag.starts_with("cannot listen on port 1000 on 127.0.0.1 (permission denied)")
                && flag.contains("; pass another --port"),
            "{flag}"
        );
        let configured = message(PortChoice::From(port));
        assert!(
            configured.contains("; set another port in ") && configured.ends_with("or pass --port"),
            "{configured}"
        );
    }

    #[test]
    fn idle_state_counts_down_and_never_expires() {
        let start = Instant::now();
        let state = IdleState {
            timeout: IdleTimeout::After(Duration::from_secs(600)),
            source: TimeoutSource::Saved,
            last_activity: start,
        };
        assert_eq!(
            state.remaining(start + Duration::from_secs(60)),
            Some(Duration::from_secs(540))
        );
        assert!(!state.expired(start + Duration::from_secs(599)));
        assert!(state.expired(start + Duration::from_secs(600)));
        let never = IdleState {
            timeout: IdleTimeout::Never,
            ..state
        };
        assert_eq!(
            never.remaining(start + Duration::from_secs(86_400 * 30)),
            None
        );
        assert!(!never.expired(start + Duration::from_secs(86_400 * 30)));
    }
}
