//! The web server's lifecycle on disk (docs/design.md §13.1–§13.2): the run directory,
//! `server.json`, the server lock, starting a detached server, health checks, stopping it, and
//! opening the browser without exposing the token on a command line.
//!
//! This module is synchronous: it is used by the CLI and by the MCP server's `open_web_ui` tool.

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::listen::{Binding, Listen, socket_path};
use crate::port::PortChoice;
use crate::text::quoted;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// How long to wait for a health check or shutdown response.
const HTTP_TIMEOUT: Duration = Duration::from_millis(500);
/// How long a newly started server has to become healthy.
const START_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait for a stopped server to exit.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to poll a browser opener for an early failure.
const OPENER_CHECK: Duration = Duration::from_secs(2);
/// Browser-launch redirect files contain the token, so they are deleted after this long.
pub const OPEN_FILE_MAX_AGE: Duration = Duration::from_secs(60);

const SERVER_FILE: &str = "server.json";
const LOCK_FILE: &str = "server.lock";
const OPEN_FILE_PREFIX: &str = "open-";

#[must_use]
pub fn run_dir(home: &Path) -> PathBuf {
    home.join("run")
}

#[must_use]
pub fn log_path(home: &Path) -> PathBuf {
    home.join("logs").join("server.log")
}

/// Create the run and log directories (mode 0700) if needed.
pub fn ensure_dirs(home: &Path) -> Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(run_dir(home))?;
    builder.create(home.join("logs"))?;
    Ok(())
}

/// Random bytes from the OS, hex-encoded.
pub fn random_hex(bytes: usize) -> Result<String> {
    let mut buffer = vec![0_u8; bytes];
    getrandom::fill(&mut buffer).map_err(|error| {
        Error::Internal(format!("the OS random number generator failed: {error}"))
    })?;
    Ok(buffer
        .iter()
        .fold(String::with_capacity(bytes * 2), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        }))
}

/// How to reach the running server, as `server.json` and the health check record it (§13.1).
///
/// Untagged, so that a TCP server's `server.json` keeps the format that older `lani` binaries
/// read, with `port`. A socket-mode one has no `port`, so older binaries cannot read it, and they
/// never send the token to a TCP port that may belong to someone else (§13.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Endpoint {
    /// A TCP port on 127.0.0.1.
    Tcp { port: u16 },
    /// A Unix socket, reached from your own machine through an SSH forward (§13.2).
    Socket {
        socket: PathBuf,
        /// The local port that the printed command and links suggest you forward from.
        browser_port: u16,
        /// This machine's name, for the printed `ssh` command; `None` if it could not be found.
        ssh_host: Option<String>,
    },
}

impl Endpoint {
    /// The port in links for the browser: the server's own, or the one forwarded to its socket.
    #[must_use]
    pub fn browser_port(&self) -> u16 {
        match self {
            Self::Tcp { port } => *port,
            Self::Socket { browser_port, .. } => *browser_port,
        }
    }

    /// Where the server listens, for status lines: `port 5264` or `socket /…/run/web.sock`.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Tcp { port } => format!("port {port}"),
            Self::Socket { socket, .. } => format!("socket {}", socket.display()),
        }
    }

    /// The machine LANI runs on, as directions given on your own machine name it.
    #[must_use]
    pub fn machine(&self) -> &str {
        match self {
            Self::Socket {
                ssh_host: Some(host),
                ..
            } => host,
            _ => "the machine LANI runs on",
        }
    }

    /// How to forward a local port to a socket-mode server, ending in a line break; `None` for
    /// a TCP server, which needs no directions.
    #[must_use]
    pub fn forward_directions(&self) -> Option<String> {
        let Self::Socket {
            socket,
            browser_port,
            ssh_host,
        } = self
        else {
            return None;
        };
        let (host, whose) = match ssh_host {
            Some(host) => (
                host.as_str(),
                format!("{host} is this machine's name; use the name you ssh to"),
            ),
            None => ("<host>", "use the name you ssh to".to_string()),
        };
        Some(format!(
            "To reach LANI from your own machine, forward a local port to it over SSH ({whose}):\n\
             ssh -L {} {host}\n",
            forward_argument(*browser_port, socket)
        ))
    }
}

/// The `-L` argument that forwards `browser_port` to `socket`, written so that the shell and then
/// ssh read back exactly that path (§13.2):
/// - `%` is written `%%`, because ssh expands `%` tokens in socket paths. It also expands `${…}`,
///   which can't be escaped, so [`crate::listen::Binding`] refuses a path containing it.
/// - A path with a `:` goes in brackets, since ssh's parser otherwise splits the argument there.
/// - Anything but letters, digits and `._/:-` puts the whole argument in single quotes.
fn forward_argument(browser_port: u16, socket: &Path) -> String {
    let path = socket.to_string_lossy().replace('%', "%%");
    let path = if path.contains(':') {
        format!("[{path}]")
    } else {
        path
    };
    let argument = format!("{browser_port}:{path}");
    let plain = argument.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b':' | b'-')
    });
    if plain {
        argument
    } else {
        format!("'{}'", argument.replace('\'', r"'\''"))
    }
}

/// This machine's name as `uname -n` prints it, the same as `hostname`: usually the name you ssh
/// to, for the printed `ssh` command (§13.2). `None` if it cannot be found, or if it isn't
/// [`plain_host`].
#[must_use]
pub fn host_name() -> Option<String> {
    let output = Command::new("uname")
        .arg("-n")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    plain_host(&String::from_utf8(output.stdout).ok()?)
}

/// `name`, trimmed, if it can go into the printed `ssh` command as it is: letters, digits, `.`, `-`
/// and `_` only, so the shell leaves it alone, and no leading `-`, which ssh would read as an
/// option.
fn plain_host(name: &str) -> Option<String> {
    let name = name.trim();
    let plain = !name.is_empty()
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'));
    plain.then(|| name.to_string())
}

/// The contents of `run/server.json`: how to reach the running server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerFile {
    pub pid: u32,
    #[serde(flatten)]
    pub endpoint: Endpoint,
    pub token: String,
    pub instance_id: String,
    pub version: String,
    pub started_at: String,
}

impl ServerFile {
    /// Read `server.json`; `None` if it is missing or unreadable (a stale or partial file).
    #[must_use]
    pub fn read(home: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(run_dir(home).join(SERVER_FILE)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Write `server.json` atomically with mode 0600.
    pub fn write(&self, home: &Path) -> Result<()> {
        let directory = run_dir(home);
        let temporary = directory.join(format!("{SERVER_FILE}.{}.tmp", self.pid));
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        let json = serde_json::to_string_pretty(self)
            .map_err(|error| Error::Internal(format!("cannot serialize server.json: {error}")))?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, directory.join(SERVER_FILE))?;
        Ok(())
    }

    /// Remove `server.json` if it still describes the server with `instance_id`.
    pub fn remove_if_ours(home: &Path, instance_id: &str) -> Result<()> {
        if Self::read(home).is_some_and(|file| file.instance_id == instance_id) {
            std::fs::remove_file(run_dir(home).join(SERVER_FILE))?;
        }
        Ok(())
    }

    /// The URL that logs the browser in and then shows `next` (a path such as `/reviews/3`).
    /// It contains the token, so it must never be put on a command line or in logs.
    #[must_use]
    pub fn login_url(&self, next: &str) -> String {
        format!(
            "http://127.0.0.1:{}/login?token={}&next={}",
            self.endpoint.browser_port(),
            self.token,
            encode_query_value(next)
        )
    }

    /// What `lani serve` prints to stdout: the sign-in link to the review list, introduced, after
    /// the directions for forwarding a port to a socket-mode server. It contains the token, so a
    /// detached server's stdout is `/dev/null`, not the log.
    #[must_use]
    pub fn sign_in_text(&self) -> String {
        format!(
            "{}To sign in, open this link; it contains the access token, so keep it private:\n{}\n",
            self.endpoint.forward_directions().unwrap_or_default(),
            self.login_url("/")
        )
    }

    /// The URL of `path` without credentials, safe to show anywhere.
    #[must_use]
    pub fn public_url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.endpoint.browser_port())
    }
}

/// An exclusive lock on `run/server.lock`, held by the running server for its whole lifetime.
/// The OS releases it however the server ends, so a free lock means no server process is alive.
pub struct ServerLock {
    _file: File,
}

impl ServerLock {
    /// Take the lock, or return `None` if another server holds it.
    pub fn try_acquire(home: &Path) -> Result<Option<Self>> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(run_dir(home).join(LOCK_FILE))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }
}

/// What `GET /api/health` reports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    pub instance_id: String,
    pub version: String,
    pub pid: u32,
    #[serde(flatten)]
    pub endpoint: Endpoint,
    pub started_at: String,
    /// The effective idle timeout, e.g. `3h` or `never`.
    pub idle_timeout: String,
    /// Where the effective idle timeout came from: `saved setting` or `--idle-timeout flag`.
    pub idle_timeout_source: String,
    /// Seconds until the server stops if nothing happens; `None` when the timeout is `never`.
    pub idle_remaining_secs: Option<u64>,
}

/// A server that answered its health check with the instance id from `server.json`.
#[derive(Clone, Debug)]
pub struct RunningServer {
    pub file: ServerFile,
    pub health: Health,
}

impl RunningServer {
    /// A warning when the running server is a different version from this binary.
    #[must_use]
    pub fn version_warning(&self) -> Option<String> {
        (self.health.version != VERSION).then(|| {
            format!(
                "The running LANI server is version {} but this lani is version {VERSION}; \
                 run `lani stop && lani open` to restart it.",
                self.health.version
            )
        })
    }
}

/// The server described by `server.json`, if it is alive and really is that server.
#[must_use]
pub fn find_running(home: &Path) -> Option<RunningServer> {
    let file = ServerFile::read(home)?;
    let (status, body) = http_call(&file.endpoint, "GET", "/api/health", &file.token).ok()?;
    if status != 200 {
        return None;
    }
    let health: Health = serde_json::from_slice(&body).ok()?;
    (health.instance_id == file.instance_id).then_some(RunningServer { file, health })
}

/// The running server, starting a detached one first if necessary.
/// Returns the server and whether it was started by this call.
pub fn ensure_running(home: &Path) -> Result<(RunningServer, bool)> {
    if let Some(server) = find_running(home) {
        return Ok((server, false));
    }
    Ok((start_detached(home, None, None, None)?, true))
}

/// Start `lani serve` as a detached process and wait for it to become healthy.
pub fn start_detached(
    home: &Path,
    port: Option<u16>,
    listen: Option<Listen>,
    idle_timeout: Option<&str>,
) -> Result<RunningServer> {
    ensure_dirs(home)?;
    // The server decides this itself. Deciding it here too reports a mistake in the configuration
    // file, or a socket path that cannot work, to whoever is starting the server, instead of only
    // in the server's log.
    Binding::choose(home, port.map(PortChoice::flag).transpose()?, listen)?;
    let log = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(log_path(home))?;
    let mut command = Command::new(executable_to_run(&std::env::current_exe()?)?);
    command.arg("serve");
    if let Some(port) = port {
        command.args(["--port", &port.to_string()]);
    }
    if let Some(listen) = listen {
        command.args(["--listen", listen.as_str()]);
    }
    if let Some(idle_timeout) = idle_timeout {
        command.args(["--idle-timeout", idle_timeout]);
    }
    command
        .env("LANI_HOME", home)
        // Like any daemon, don't pin the starting session's directory, which may be a temporary
        // worktree that is deleted while the server keeps running.
        .current_dir("/")
        .stdin(Stdio::null())
        // The server prints only its sign-in link to stdout, and the link holds the token, which
        // must never reach the log (§13.2).
        .stdout(Stdio::null())
        .stderr(log)
        // A new process group, so the server survives the terminal or Claude session that
        // started it; the server also ignores SIGHUP.
        .process_group(0);
    wait_until_running(home, &mut command)
}

/// The `lani` binary to start the server from, given this process's [`std::env::current_exe`].
///
/// On Linux that reads `/proc/self/exe`, which shows `<path> (deleted)` once the binary is
/// replaced on disk, as `cargo install` does. The path without the suffix is the replacement, so
/// run that; [`RunningServer::version_warning`] covers a version difference.
fn executable_to_run(current: &Path) -> Result<PathBuf> {
    if current.exists() {
        return Ok(current.to_path_buf());
    }
    if let Some(original) = current
        .as_os_str()
        .as_bytes()
        .strip_suffix(b" (deleted)")
        .map(|original| Path::new(OsStr::from_bytes(original)))
        && original.exists()
    {
        return Ok(original.to_path_buf());
    }
    Err(Error::Unavailable(format!(
        "cannot start the LANI server: the lani binary this process was started from, {}, has been \
         removed or replaced. Restart this process (for `lani mcp`, reconnect LANI with /mcp in \
         Claude Code), or start the server with `lani open`.",
        quoted(&current.display().to_string())
    )))
}

/// Start the server with `command` and wait for it to answer its health check.
fn wait_until_running(home: &Path, command: &mut Command) -> Result<RunningServer> {
    let deadline = Instant::now() + START_TIMEOUT;
    let mut child = command.spawn()?;
    loop {
        if let Some(server) = find_running(home) {
            // Reap the child when it eventually exits, so it never lingers as a zombie.
            std::thread::spawn(move || child.wait());
            return Ok(server);
        }
        match child.try_wait()? {
            Some(status) if !status.success() => {
                return Err(Error::Unavailable(format!(
                    "the LANI server exited during startup ({status}); see {}",
                    log_path(home).display()
                )));
            }
            // A successful early exit means another server held the lock. If that server was
            // stopping, the lock comes free with no server running, so start one again.
            Some(_) if ServerLock::try_acquire(home)?.is_some() => child = command.spawn()?,
            // Otherwise keep waiting: for this server, or for the one holding the lock.
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err(Error::Unavailable(format!(
                "the LANI server did not start within {}s; see {}",
                START_TIMEOUT.as_secs(),
                log_path(home).display()
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// What `stop` stopped.
#[derive(Clone, Debug)]
pub struct Stopped {
    pub server: RunningServer,
    /// A server that another command, such as `lani open`, started as this one ended. It took
    /// the lock first, and it is left running.
    pub successor: Option<RunningServer>,
}

/// Ask the running server to stop and wait for it to go away.
/// Returns what was stopped, or `None` if no server was running.
pub fn stop(home: &Path) -> Result<Option<Stopped>> {
    let Some(server) = find_running(home) else {
        return Ok(None);
    };
    let (status, _) = http_call(
        &server.file.endpoint,
        "POST",
        "/api/shutdown",
        &server.file.token,
    )
    .map_err(|error| Error::Unavailable(format!("could not reach the LANI server: {error}")))?;
    if status != 200 {
        return Err(Error::Unavailable(format!(
            "the LANI server refused to stop (HTTP {status})"
        )));
    }
    // Wait for the server process to end, cleanup and all: the server lock comes free only then,
    // or when it dies. The health check fails sooner, as soon as the server stops listening.
    let deadline = Instant::now() + STOP_TIMEOUT;
    let _lock = loop {
        if let Some(lock) = ServerLock::try_acquire(home)? {
            break lock;
        }
        // Another server answering means it holds the lock, so this one has ended, and its
        // cleanup is done: the successor took over server.json.
        if let Some(successor) = find_running(home)
            && successor.file.instance_id != server.file.instance_id
        {
            return Ok(Some(Stopped {
                server,
                successor: Some(successor),
            }));
        }
        if Instant::now() >= deadline {
            return Err(Error::Unavailable(format!(
                "the LANI server (pid {}) did not stop within {}s",
                server.health.pid,
                STOP_TIMEOUT.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    // A server that died before its cleanup leaves these behind. While this holds the lock, no
    // other server can be running to own them.
    ServerFile::remove_if_ours(home, &server.file.instance_id)?;
    remove_open_files(home, Duration::ZERO);
    remove_socket(home)?;
    Ok(Some(Stopped {
        server,
        successor: None,
    }))
}

/// Remove a socket-mode server's socket, if one is there. Only for a holder of the server lock,
/// for whom no other server can be running to own it (§13.1).
pub fn remove_socket(home: &Path) -> Result<()> {
    let path = socket_path(home);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_socket() => Ok(std::fs::remove_file(&path)?),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Open `next` in the browser. The login URL is written to a private redirect file and the
/// browser is pointed at that file, because process arguments are visible to other users.
pub fn open_in_browser(home: &Path, server: &ServerFile, next: &str) -> Result<()> {
    sweep_open_files(home);
    let path = run_dir(home).join(format!("{OPEN_FILE_PREFIX}{}.html", random_hex(8)?));
    let url = escape_html(&server.login_url(next));
    let page = format!(
        "<!doctype html>\n<meta charset=\"utf-8\">\n<meta http-equiv=\"refresh\" content=\"0;url={url}\">\n\
         <title>Opening LANI</title>\n<p><a href=\"{url}\">Open LANI</a></p>\n"
    );
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(page.as_bytes())?;
    drop(file);

    let opener = std::env::var("BROWSER")
        .ok()
        .filter(|browser| !browser.is_empty())
        .unwrap_or_else(|| {
            if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            }
            .to_string()
        });
    let mut child = Command::new(&opener)
        .arg(&path)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| Error::Unavailable(format!("could not run {opener}: {error}")))?;
    // Openers usually exit at once; report an early failure (e.g. no display), otherwise let a
    // thread reap the process whenever it finishes.
    let deadline = Instant::now() + OPENER_CHECK;
    loop {
        match child.try_wait()? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => {
                return Err(Error::Unavailable(format!(
                    "{opener} could not open a browser ({status})"
                )));
            }
            None if Instant::now() >= deadline => {
                std::thread::spawn(move || child.wait());
                return Ok(());
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Delete browser-launch redirect files older than [`OPEN_FILE_MAX_AGE`]. Best effort: a file
/// that cannot be inspected or removed is left for the next sweep.
pub fn sweep_open_files(home: &Path) {
    remove_open_files(home, OPEN_FILE_MAX_AGE);
}

/// Delete browser-launch redirect files at least `min_age` old (all of them for
/// `Duration::ZERO`, as when the server stops and their tokens become useless).
pub fn remove_open_files(home: &Path, min_age: Duration) {
    let Ok(entries) = std::fs::read_dir(run_dir(home)) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with(OPEN_FILE_PREFIX) && name.ends_with(".html")) {
            continue;
        }
        let expired = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= min_age);
        if expired {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// A minimal blocking HTTP/1.1 request to the local LANI server, on its port or its socket.
/// Returns the status code and body. Only used for LANI's own small, fixed-length JSON responses.
fn http_call(
    endpoint: &Endpoint,
    method: &str,
    path: &str,
    token: &str,
) -> std::io::Result<(u16, Vec<u8>)> {
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {token}\r\n\
         Content-Length: 0\r\nConnection: close\r\n\r\n",
        endpoint.browser_port()
    );
    let response = match endpoint {
        Endpoint::Tcp { port } => {
            let address = SocketAddr::from(([127, 0, 0, 1], *port));
            let stream = TcpStream::connect_timeout(&address, HTTP_TIMEOUT)?;
            stream.set_read_timeout(Some(HTTP_TIMEOUT * 4))?;
            stream.set_write_timeout(Some(HTTP_TIMEOUT))?;
            exchange(stream, &request)?
        }
        Endpoint::Socket { socket, .. } => {
            // std has no connect timeout for Unix sockets. The kernel completes the connection
            // without the server's help, so this waits only while the listen queue is full.
            let stream = UnixStream::connect(socket)?;
            stream.set_read_timeout(Some(HTTP_TIMEOUT * 4))?;
            stream.set_write_timeout(Some(HTTP_TIMEOUT))?;
            exchange(stream, &request)?
        }
    };
    parse_http_response(&response)
}

/// Send `request` and read the response until the server closes the connection.
fn exchange(mut stream: impl Read + Write, request: &str) -> std::io::Result<Vec<u8>> {
    stream.write_all(request.as_bytes())?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    Ok(response)
}

fn parse_http_response(response: &[u8]) -> std::io::Result<(u16, Vec<u8>)> {
    let invalid =
        |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_string());
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| invalid("HTTP response without a header terminator"))?;
    let head =
        std::str::from_utf8(&response[..split]).map_err(|_| invalid("non-UTF-8 HTTP headers"))?;
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        return Err(invalid("unexpected chunked HTTP response"));
    }
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| invalid("HTTP response without a status code"))?;
    Ok((status, response[split + 4..].to_vec()))
}

/// Percent-encode a query parameter value, keeping unreserved characters and `/`.
fn encode_query_value(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replaced_binary_is_run_from_its_original_path() {
        let directory = tempfile::tempdir().unwrap();
        let installed = directory.path().join("lani");
        std::fs::write(&installed, "").unwrap();
        assert_eq!(executable_to_run(&installed).unwrap(), installed);

        // What Linux reports for a binary that `cargo install` renamed a new file over.
        let replaced = directory.path().join("lani (deleted)");
        assert_eq!(executable_to_run(&replaced).unwrap(), installed);

        std::fs::remove_file(&installed).unwrap();
        for gone in [&replaced, &installed] {
            match executable_to_run(gone) {
                Err(Error::Unavailable(message)) => assert!(
                    message.contains("has been removed or replaced")
                        && message.contains(&quoted(&gone.display().to_string())),
                    "{message}"
                ),
                other => panic!("expected Unavailable, got {other:?}"),
            }
        }
    }

    #[test]
    fn random_hex_has_the_requested_length() {
        let token = random_hex(32).unwrap();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(token, random_hex(32).unwrap());
    }

    #[test]
    fn login_url_encodes_next() {
        let file = ServerFile {
            pid: 1,
            endpoint: Endpoint::Tcp { port: 4242 },
            token: "abc".into(),
            instance_id: "i".into(),
            version: "v".into(),
            started_at: "t".into(),
        };
        assert_eq!(
            file.login_url("/reviews/3?status=all&x=1#c2"),
            "http://127.0.0.1:4242/login?token=abc&next=/reviews/3%3Fstatus%3Dall%26x%3D1%23c2"
        );
    }

    #[test]
    fn server_json_keeps_its_tcp_format_and_leaves_port_out_in_socket_mode() {
        // As the lani versions before socket mode write and read it.
        let older = r#"{"pid": 7, "port": 5264, "token": "t", "instance_id": "i",
                        "version": "0.1.0", "started_at": "s"}"#;
        let file: ServerFile = serde_json::from_str(older).unwrap();
        assert_eq!(file.endpoint, Endpoint::Tcp { port: 5264 });
        assert_eq!(
            serde_json::to_value(&file).unwrap(),
            serde_json::from_str::<serde_json::Value>(older).unwrap(),
            "unchanged, so older binaries still read it"
        );

        let socket = ServerFile {
            endpoint: Endpoint::Socket {
                socket: "/home/you/.local/share/lani/run/web.sock".into(),
                browser_port: 5300,
                ssh_host: Some("devbox".into()),
            },
            ..file
        };
        let written = serde_json::to_value(&socket).unwrap();
        assert!(
            written.get("port").is_none(),
            "older binaries must fail to read it: {written}"
        );
        assert_eq!(
            written["socket"],
            "/home/you/.local/share/lani/run/web.sock"
        );
        assert_eq!(written["browser_port"], 5300);
        assert_eq!(written["ssh_host"], "devbox");
        assert_eq!(
            serde_json::from_value::<ServerFile>(written).unwrap(),
            socket
        );
    }

    #[test]
    fn a_socket_mode_server_is_reached_through_a_forwarded_port() {
        let file = ServerFile {
            pid: 1,
            endpoint: Endpoint::Socket {
                socket: "/home/you/.local/share/lani/run/web.sock".into(),
                browser_port: 5300,
                ssh_host: Some("devbox".into()),
            },
            token: "abc".into(),
            instance_id: "i".into(),
            version: "v".into(),
            started_at: "t".into(),
        };
        assert_eq!(
            file.sign_in_text(),
            "To reach LANI from your own machine, forward a local port to it over SSH (devbox is \
             this machine's name; use the name you ssh to):\n\
             ssh -L 5300:/home/you/.local/share/lani/run/web.sock devbox\n\
             To sign in, open this link; it contains the access token, so keep it private:\n\
             http://127.0.0.1:5300/login?token=abc&next=/\n"
        );
        assert_eq!(
            file.public_url("/reviews/3"),
            "http://127.0.0.1:5300/reviews/3"
        );
        assert_eq!(file.endpoint.machine(), "devbox");
        assert_eq!(
            file.endpoint.describe(),
            "socket /home/you/.local/share/lani/run/web.sock"
        );

        let unnamed = Endpoint::Socket {
            socket: "/s".into(),
            browser_port: 5264,
            ssh_host: None,
        };
        assert_eq!(
            unnamed.forward_directions().unwrap(),
            "To reach LANI from your own machine, forward a local port to it over SSH (use the \
             name you ssh to):\nssh -L 5264:/s <host>\n"
        );
        assert_eq!(unnamed.machine(), "the machine LANI runs on");

        let tcp = Endpoint::Tcp { port: 5264 };
        assert_eq!(tcp.forward_directions(), None);
        assert_eq!(tcp.describe(), "port 5264");
    }

    #[test]
    fn the_forward_argument_reads_back_as_the_socket_path() {
        let argument = |path: &str| forward_argument(5264, Path::new(path));
        assert_eq!(
            argument("/home/you/.local/share/lani/run/web.sock"),
            "5264:/home/you/.local/share/lani/run/web.sock",
            "a plain path as it is"
        );
        assert_eq!(
            argument("/home/a b/run/web.sock"),
            "'5264:/home/a b/run/web.sock'"
        );
        assert_eq!(
            argument("/home/it's/run/web.sock"),
            r"'5264:/home/it'\''s/run/web.sock'"
        );
        assert_eq!(
            argument("/home/a:b/run/web.sock"),
            "'5264:[/home/a:b/run/web.sock]'",
            "ssh splits at a bare colon; the brackets make the shell quote it too"
        );
        assert_eq!(
            argument("/home/50%/run/web.sock"),
            "'5264:/home/50%%/run/web.sock'"
        );
        assert_eq!(
            argument("/x/$(id)`id`;/web.sock"),
            "'5264:/x/$(id)`id`;/web.sock'"
        );
    }

    #[test]
    fn a_host_name_goes_into_the_command_only_if_plain() {
        assert_eq!(
            plain_host("devbox.example.com\n").as_deref(),
            Some("devbox.example.com")
        );
        assert_eq!(plain_host("build_07-a").as_deref(), Some("build_07-a"));
        for unusable in ["", "-devbox", "a b", "a;b", "a$b", "naïve"] {
            assert_eq!(plain_host(unusable), None, "{unusable:?}");
        }
    }

    #[test]
    fn parses_http_responses() {
        let response = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}";
        assert_eq!(
            parse_http_response(response).unwrap(),
            (200, b"{}".to_vec())
        );
        assert!(parse_http_response(b"garbage").is_err());
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n";
        assert!(parse_http_response(chunked).is_err());
    }

    #[test]
    fn escapes_html_attributes() {
        assert_eq!(escape_html("a&b\"<>"), "a&amp;b&quot;&lt;&gt;");
    }
}
