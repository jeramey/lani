//! The web server's lifecycle on disk (docs/design.md §13.1–§13.2): the run directory,
//! `server.json`, the server lock, starting a detached server, health checks, stopping it, and
//! opening the browser without exposing the token on a command line.
//!
//! This module is synchronous: it is used by the CLI and by the MCP server's `open_web_ui` tool.

use std::fmt::Write as _;
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

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

/// The contents of `run/server.json`: how to reach the running server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerFile {
    pub pid: u32,
    pub port: u16,
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
            self.port,
            self.token,
            encode_query_value(next)
        )
    }

    /// The URL of `path` without credentials, safe to show anywhere.
    #[must_use]
    pub fn public_url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

/// An exclusive lock on `run/server.lock`, held by the running server for its whole lifetime.
/// The OS releases it if the server crashes.
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
    pub port: u16,
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
    let (status, body) = http_call(file.port, "GET", "/api/health", &file.token).ok()?;
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
    Ok((start_detached(home, None, None)?, true))
}

/// Start `lani serve` as a detached process and wait for it to become healthy.
pub fn start_detached(
    home: &Path,
    port: Option<u16>,
    idle_timeout: Option<&str>,
) -> Result<RunningServer> {
    ensure_dirs(home)?;
    let log = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(log_path(home))?;
    let mut command = Command::new(std::env::current_exe()?);
    command.arg("serve");
    if let Some(port) = port {
        command.args(["--port", &port.to_string()]);
    }
    if let Some(idle_timeout) = idle_timeout {
        command.args(["--idle-timeout", idle_timeout]);
    }
    command
        .env("LANI_HOME", home)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        // A new process group, so the server survives the terminal or Claude session that
        // started it; the server also ignores SIGHUP.
        .process_group(0);
    let child = command.spawn()?;
    wait_until_running(home, child)
}

fn wait_until_running(home: &Path, mut child: Child) -> Result<RunningServer> {
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Some(server) = find_running(home) {
            // Reap the child when it eventually exits, so it never lingers as a zombie.
            std::thread::spawn(move || child.wait());
            return Ok(server);
        }
        if let Some(status) = child.try_wait()?
            && !status.success()
        {
            return Err(Error::Unavailable(format!(
                "the LANI server exited during startup ({status}); see {}",
                log_path(home).display()
            )));
        }
        // A successful early exit means another server holds the lock; keep waiting for it.
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

/// Ask the running server to stop and wait for it to go away.
/// Returns the server that was stopped, or `None` if none was running.
pub fn stop(home: &Path) -> Result<Option<RunningServer>> {
    let Some(server) = find_running(home) else {
        return Ok(None);
    };
    let (status, _) = http_call(
        server.file.port,
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
    let deadline = Instant::now() + STOP_TIMEOUT;
    while find_running(home).is_some() {
        if Instant::now() >= deadline {
            return Err(Error::Unavailable(format!(
                "the LANI server (pid {}) did not stop within {}s",
                server.health.pid,
                STOP_TIMEOUT.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(Some(server))
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

/// A minimal blocking HTTP/1.1 request to the local LANI server. Returns the status code and
/// body. Only used for LANI's own small, fixed-length JSON responses.
fn http_call(port: u16, method: &str, path: &str, token: &str) -> std::io::Result<(u16, Vec<u8>)> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&address, HTTP_TIMEOUT)?;
    stream.set_read_timeout(Some(HTTP_TIMEOUT * 4))?;
    stream.set_write_timeout(Some(HTTP_TIMEOUT))?;
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n\
         Content-Length: 0\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    parse_http_response(&response)
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
            port: 4242,
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
