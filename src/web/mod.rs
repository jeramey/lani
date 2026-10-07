//! The local web UI server (docs/design.md §13).
//!
//! M2 serves server-rendered pages and plain HTML forms: every action is a POST that redirects
//! back to the page (or re-renders it with an error), so no JavaScript is needed yet.

mod actions;
mod auth;
mod pages;
mod views;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::{get, post};
use tokio::sync::watch;

use crate::daemon::{self, ServerFile};
use crate::error::{Error, Result};
use crate::service::Service;
use crate::settings::IdleTimeout;

pub use auth::{allowed_host, is_safe_next, same_origin};

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

/// Fixed facts about this server instance.
#[derive(Clone, Debug)]
pub struct Instance {
    pub token: String,
    pub instance_id: String,
    pub port: u16,
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
    idle: Mutex<IdleState>,
    shutdown: watch::Sender<bool>,
}

impl AppState {
    #[must_use]
    pub fn new(
        service: Service,
        instance: Instance,
        timeout: IdleTimeout,
        source: TimeoutSource,
    ) -> Self {
        let (shutdown, _) = watch::channel(false);
        Self {
            inner: Arc::new(Inner {
                service: Mutex::new(service),
                instance,
                idle: Mutex::new(IdleState {
                    timeout,
                    source,
                    last_activity: Instant::now(),
                }),
                shutdown,
            }),
        }
    }

    #[must_use]
    pub fn instance(&self) -> &Instance {
        &self.inner.instance
    }

    /// Run a service operation on the blocking thread pool; SQLite and git calls block.
    pub async fn call<T, F>(&self, operation: F) -> Result<T>
    where
        F: FnOnce(&mut Service) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let mut service = inner.service.lock().unwrap_or_else(PoisonError::into_inner);
            operation(&mut service)
        })
        .await
        .map_err(|error| Error::Internal(format!("a request handler failed: {error}")))?
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

    pub fn request_shutdown(&self) {
        self.inner.shutdown.send_replace(true);
    }

    /// Resolves once a shutdown has been requested.
    pub async fn shutdown_requested(&self) {
        let mut receiver = self.inner.shutdown.subscribe();
        let _ = receiver.wait_for(|requested| *requested).await;
    }
}

/// All routes, behind the authentication and security-header middleware.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(pages::index))
        .route("/reviews/{review_id}", get(pages::review))
        .route("/reviews/{review_id}/issues/{issue_id}", get(pages::issue))
        .route("/issues/{issue_id}", get(pages::issue_redirect))
        .route(
            "/settings",
            get(pages::settings).post(actions::save_settings),
        )
        .route("/static/lani.css", get(pages::stylesheet))
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
        .route("/reviews/{review_id}/reopen", post(actions::reopen_review))
        .route(
            "/reviews/{review_id}/issues/{issue_id}/comments",
            post(actions::comment_on_issue),
        )
        .route(
            "/reviews/{review_id}/issues/{issue_id}/status",
            post(actions::set_issue_status),
        )
        .route("/comments/{comment_id}/edit", post(actions::edit_comment))
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
    pub port: Option<u16>,
    /// Overrides the saved idle timeout for this run.
    pub idle_timeout: Option<IdleTimeout>,
}

/// Run the web server until it is stopped. Exits quietly if another server is already running.
pub fn serve(options: &ServeOptions) -> Result<()> {
    daemon::ensure_dirs(&options.home)?;
    let Some(_lock) = daemon::ServerLock::try_acquire(&options.home)? else {
        eprintln!("A LANI server is already running; use `lani open` to reach it.");
        return Ok(());
    };
    let service = Service::open(&crate::paths::db_path(&options.home))?;
    let (timeout, source) = match options.idle_timeout {
        Some(timeout) => (timeout, TimeoutSource::Flag),
        None => (service.idle_timeout()?, TimeoutSource::Saved),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    runtime.block_on(run(options, service, timeout, source))
}

async fn run(
    options: &ServeOptions,
    service: Service,
    timeout: IdleTimeout,
    source: TimeoutSource,
) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", options.port.unwrap_or(0))).await?;
    let port = listener.local_addr()?.port();
    let instance = Instance {
        token: daemon::random_hex(32)?,
        instance_id: daemon::random_hex(16)?,
        port,
        pid: std::process::id(),
        started_at: crate::time::now(),
        home: options.home.clone(),
    };
    let server_file = ServerFile {
        pid: instance.pid,
        port,
        token: instance.token.clone(),
        instance_id: instance.instance_id.clone(),
        version: daemon::VERSION.to_string(),
        started_at: instance.started_at.clone(),
    };
    let state = AppState::new(service, instance, timeout, source);
    server_file.write(&options.home)?;
    eprintln!(
        "LANI server listening on 127.0.0.1:{port} (pid {})",
        server_file.pid
    );

    tokio::spawn(housekeeping(state.clone()));
    tokio::spawn(handle_signals(state.clone()));

    let served = axum::serve(listener, router(state.clone()))
        .with_graceful_shutdown({
            let state = state.clone();
            async move { state.shutdown_requested().await }
        })
        .await;
    ServerFile::remove_if_ours(&options.home, &server_file.instance_id)?;
    daemon::remove_open_files(&options.home, Duration::ZERO);
    eprintln!("LANI server stopped");
    served.map_err(Error::Io)
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
            state.request_shutdown();
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
    state.request_shutdown();
}

#[cfg(test)]
mod tests {
    use super::*;

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
