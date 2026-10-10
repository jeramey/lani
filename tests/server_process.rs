//! Process tests: the real `lani` binary starting, finding, and stopping its web server.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Command, Output, Stdio};

use common::{Forward, TestHome};

fn lani(home: &TestHome, args: &[&str]) -> Output {
    common::output(
        Command::new(env!("CARGO_BIN_EXE_lani"))
            .args(args)
            .envs(common::lani_env(&home.path)),
    )
}

fn stdout(output: &Output) -> String {
    assert!(
        output.status.success(),
        "lani failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .trim()
        .to_string()
}

/// Stops the test's server even if an assertion fails part-way.
struct StopOnDrop<'a>(&'a TestHome);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        let _ = lani(self.0, &["stop"]);
    }
}

/// The port in a URL like `http://127.0.0.1:PORT/...`.
fn port_of(url: &str) -> u16 {
    url.trim_start_matches("http://127.0.0.1:")
        .split('/')
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

/// `GET path_and_query` against the server, returning the raw response head.
fn http_get(port: u16, path_and_query: &str) -> String {
    http_get_with(port, path_and_query, "")
}

/// `GET path_and_query` with a session cookie, `name=value`.
fn http_get_signed_in(port: u16, path_and_query: &str, cookie: &str) -> String {
    http_get_with(port, path_and_query, &format!("Cookie: {cookie}\r\n"))
}

fn http_get_with(port: u16, path_and_query: &str, headers: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        stream,
        "GET {path_and_query} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{headers}Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

#[test]
fn open_starts_a_server_once_and_stop_removes_it() {
    let home = TestHome::new();
    let _stop = StopOnDrop(&home);

    let first = stdout(&lani(
        &home,
        &["open", "--no-browser", "--print-url", "--review", "3"],
    ));
    assert!(first.contains("/login?token="), "{first}");
    assert!(first.ends_with("&next=/reviews/3"), "{first}");
    let port = port_of(&first);

    let second = stdout(&lani(&home, &["open", "--no-browser", "--print-url"]));
    assert_eq!(port_of(&second), port, "the running server is reused");

    let status = stdout(&lani(&home, &["status"]));
    assert!(status.contains(&format!("port {port}")), "{status}");
    assert!(
        status.contains("Idle timeout 3h (saved setting)"),
        "{status}"
    );

    let server_file = home.path.join("run/server.json");
    assert!(server_file.exists());
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(&server_file).unwrap().permissions(),
    );
    assert_eq!(mode & 0o777, 0o600, "server.json holds the token");

    // A redirect file written by `open` (as with a browser) is removed when the server stops.
    std::fs::write(home.path.join("run/open-test.html"), "token").unwrap();
    let stopped = stdout(&lani(&home, &["stop"]));
    assert!(stopped.starts_with("Stopped the LANI server"), "{stopped}");
    assert!(!server_file.exists(), "server.json is removed on shutdown");
    assert!(
        !home.path.join("run/open-test.html").exists(),
        "redirect files are removed on shutdown"
    );
    assert_eq!(
        stdout(&lani(&home, &["status"])),
        "No LANI server is running."
    );
    assert_eq!(
        stdout(&lani(&home, &["stop"])),
        "No LANI server is running."
    );
}

#[test]
fn open_reaches_views_files_and_lines() {
    let home = TestHome::new();
    let _stop = StopOnDrop(&home);
    let url = |args: &[&str]| {
        let mut all = vec!["open", "--no-browser", "--print-url", "--review", "3"];
        all.extend_from_slice(args);
        stdout(&lani(&home, &all))
    };
    // `next` is percent-encoded, so the `#` of a line reaches the sign-in page intact.
    assert!(
        url(&["--file", "src/a b.rs:12"]).ends_with("&next=/reviews/3/files/src/a%2520b.rs%23L12"),
        "a file alone opens the Files view at the line"
    );
    assert!(url(&["--view", "files"]).ends_with("&next=/reviews/3/files/"));
    assert!(
        url(&["--view", "changes", "--file", "a.rs"])
            .ends_with("&next=/reviews/3/changes%23file-612e7273")
    );
    assert!(
        url(&["--file", "notes:v2.txt"]).ends_with("&next=/reviews/3/files/notes%253Av2.txt"),
        "a colon not followed by digits is part of the path"
    );

    let without_review = lani(&home, &["open", "--no-browser", "--file", "a.rs"]);
    assert!(!without_review.status.success(), "--file needs --review");
    let overview_file = lani(
        &home,
        &[
            "open",
            "--no-browser",
            "--review",
            "3",
            "--view",
            "overview",
            "--file",
            "a.rs",
        ],
    );
    assert!(!overview_file.status.success());
    assert!(
        String::from_utf8_lossy(&overview_file.stderr).contains("the overview shows no file"),
        "{}",
        String::from_utf8_lossy(&overview_file.stderr)
    );
}

#[test]
fn concurrent_opens_start_exactly_one_server() {
    let home = TestHome::new();
    let _stop = StopOnDrop(&home);
    // Start all four before waiting for any, so they race to start a server.
    let children: Vec<_> = (0..4)
        .map(|_| {
            common::spawn(
                Command::new(env!("CARGO_BIN_EXE_lani"))
                    .args(["open", "--no-browser", "--print-url"])
                    .envs(common::lani_env(&home.path))
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped()),
            )
        })
        .collect();
    let ports: Vec<u16> = children
        .into_iter()
        .map(|child| port_of(&stdout(&child.wait_with_output().unwrap())))
        .collect();
    assert!(ports.iter().all(|port| *port == ports[0]), "{ports:?}");
}

#[test]
fn the_login_url_signs_the_browser_in() {
    let home = TestHome::new();
    let _stop = StopOnDrop(&home);
    let url = stdout(&lani(
        &home,
        &["open", "--no-browser", "--print-url", "--issue", "7"],
    ));
    let port = port_of(&url);
    let path = url.split_once(&format!(":{port}")).unwrap().1;

    let response = http_get(port, path);
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(
        response.contains(&format!("set-cookie: lani_{port}=")),
        "{response}"
    );
    assert!(response.contains("SameSite=Strict"), "{response}");
    assert!(
        response.contains("content=\"0;url=/issues/7\""),
        "{response}"
    );

    let without_cookie = http_get(port, "/");
    assert!(
        without_cookie.starts_with("HTTP/1.1 401"),
        "{without_cookie}"
    );
}

#[test]
fn serve_detach_applies_the_idle_timeout_flag_for_that_run() {
    let home = TestHome::new();
    let _stop = StopOnDrop(&home);
    let output = lani(&home, &["serve", "--detach", "--idle-timeout", "10m"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status = stdout(&lani(&home, &["status"]));
    assert!(
        status.contains("Idle timeout 10m (--idle-timeout flag)"),
        "{status}"
    );

    let again = lani(&home, &["serve", "--detach", "--idle-timeout", "1h"]);
    assert!(again.status.success());
    assert!(
        String::from_utf8_lossy(&again.stderr).contains("already running"),
        "a second detach does not start another server"
    );

    let invalid = lani(&home, &["serve", "--detach", "--idle-timeout", "1m"]);
    assert!(!invalid.status.success(), "too-short timeouts are rejected");
}

#[test]
fn serve_prints_the_sign_in_link_but_never_logs_it() {
    let home = TestHome::new();
    let _stop = StopOnDrop(&home);
    let mut foreground = common::spawn(
        Command::new(env!("CARGO_BIN_EXE_lani"))
            .arg("serve")
            .envs(common::lani_env(&home.path))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()),
    );
    let mut printed =
        std::io::BufRead::lines(std::io::BufReader::new(foreground.stdout.take().unwrap()));
    let introduction = printed.next().unwrap().unwrap();
    assert!(introduction.contains("keep it private"), "{introduction}");
    let link = printed.next().unwrap().unwrap();
    assert!(
        link.starts_with("http://127.0.0.1:") && link.contains("/login?token="),
        "{link}"
    );
    stdout(&lani(&home, &["stop"]));
    assert!(foreground.wait().unwrap().success());

    // In the background, the link is printed by `lani serve --detach`, and the server's own
    // output goes to its log, which must have none of it.
    let printed = stdout(&lani(&home, &["serve", "--detach"]));
    let (introduction, link) = printed.split_once('\n').unwrap();
    assert!(introduction.contains("keep it private"), "{printed}");
    let token = link
        .split("token=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap();
    assert_eq!(token.len(), 64, "{link}");
    let again = stdout(&lani(&home, &["serve", "--detach"]));
    assert_eq!(again, printed, "the running server's link, again");
    let foreground = stdout(&lani(&home, &["serve"]));
    assert_eq!(
        foreground, printed,
        "in the foreground too, beside the running server"
    );
    // The configuration file is read only to start a server, so a mistake in it doesn't stop
    // either form naming the running one.
    home.write_config("prot = 1\n");
    for command in [&["serve", "--detach"][..], &["serve"]] {
        assert_eq!(stdout(&lani(&home, command)), printed, "{command:?}");
    }
    let log = std::fs::read_to_string(home.path.join("logs/server.log")).unwrap();
    assert!(log.contains("LANI server listening on"), "{log}");
    assert!(
        !log.contains(token) && !log.contains("sign in"),
        "nothing of the sign-in text: {log}"
    );
}

#[test]
fn status_prints_the_running_servers_sign_in_url_and_never_starts_one() {
    let home = TestHome::new();
    let _stop = StopOnDrop(&home);
    let none = lani(&home, &["status", "--print-url"]);
    assert_eq!(none.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&none.stdout),
        "",
        "nothing on stdout"
    );
    assert!(
        String::from_utf8_lossy(&none.stderr).contains("No LANI server is running"),
        "{}",
        String::from_utf8_lossy(&none.stderr)
    );
    assert!(
        !home.path.join("run/server.json").exists(),
        "no server was started"
    );

    let opened = stdout(&lani(&home, &["open", "--no-browser", "--print-url"]));
    assert_eq!(
        stdout(&lani(&home, &["status", "--print-url"])),
        opened,
        "only the URL, the same one `lani open` prints"
    );
}

#[test]
fn the_server_ignores_sighup_and_stops_cleanly_on_sigterm() {
    let home = TestHome::new();
    let _stop = StopOnDrop(&home);
    stdout(&lani(&home, &["open", "--no-browser", "--print-url"]));
    let server_file = home.path.join("run/server.json");
    let pid: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&server_file).unwrap()).unwrap();
    let pid = pid["pid"].to_string();

    let hup = common::output(Command::new("kill").args(["-HUP", &pid]));
    assert!(hup.status.success());
    std::thread::sleep(std::time::Duration::from_millis(300));
    let status = stdout(&lani(&home, &["status"]));
    assert!(
        status.contains(&format!("pid {pid}")),
        "still running after SIGHUP: {status}"
    );

    let term = common::output(Command::new("kill").args(["-TERM", &pid]));
    assert!(term.status.success());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while server_file.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "server.json not removed after SIGTERM"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(
        stdout(&lani(&home, &["status"])),
        "No LANI server is running."
    );
}

#[test]
fn the_server_does_not_depend_on_the_directory_it_was_started_from() {
    let home = TestHome::new();
    let _stop = StopOnDrop(&home);
    let repo = common::TestRepo::new(&[("a.txt", "a\n")]);
    let mut service = lani::service::Service::open(&home.db_path()).unwrap();
    service
        .start_review(&lani::service::StartReview {
            repo_path: &repo.path,
            title: "t",
            author: lani::model::Actor::Reviewer,
            base_mode: lani::base::BaseMode::None,
            summary: None,
            head_ref: None,
        })
        .unwrap();
    drop(service);

    // Start the server from a directory that is then deleted, as when the Claude session that
    // started it ends and its temporary worktree is removed.
    let session_dir = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(session_dir.path())
        .unwrap()
        .join("session");
    std::fs::create_dir(&cwd).unwrap();
    let opened = common::output(
        Command::new(env!("CARGO_BIN_EXE_lani"))
            .args(["open", "--no-browser", "--print-url"])
            .current_dir(&cwd)
            .envs(common::lani_env(&home.path)),
    );
    let port = port_of(&stdout(&opened));
    std::fs::remove_dir(&cwd).unwrap();

    let token = serde_json::from_str::<serde_json::Value>(
        &std::fs::read_to_string(home.path.join("run/server.json")).unwrap(),
    )
    .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        stream,
        "GET /reviews/1 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n\
         Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "{}",
        &response[..response.len().min(600)]
    );
}

#[test]
fn wait_exits_when_it_is_the_roles_turn_or_the_review_closes_or_time_runs_out() {
    let home = TestHome::new();
    let repo = common::TestRepo::new(&[("a.txt", "a\n")]);
    let mut service = lani::service::Service::open(&home.db_path()).unwrap();
    service
        .start_review(&lani::service::StartReview {
            repo_path: &repo.path,
            title: "t",
            author: lani::model::Actor::Reviewer,
            base_mode: lani::base::BaseMode::None,
            summary: None,
            head_ref: None,
        })
        .unwrap();
    let wait = |role: &str, extra: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lani"));
        command
            .args(["wait", "--review", "1", "--for", role])
            .args(extra)
            .envs(common::lani_env(&home.path))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        common::spawn(&mut command)
    };

    // Not the implementer's turn yet: the wait blocks until the human hands it the review.
    let waiting = wait("implementer", &[]);
    std::thread::sleep(std::time::Duration::from_millis(1500));
    service
        .hand_off(
            lani::model::Actor::Human,
            1,
            Some(lani::model::Actor::Implementer),
            Some("Over to you."),
        )
        .unwrap();
    let output = waiting.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{text}");
    assert!(
        text.starts_with("It is your turn (implementer) on review 1 \"t\"."),
        "{text}"
    );
    assert!(text.contains("│ Over to you."), "{text}");

    let timed_out = wait("reviewer", &["--timeout", "1s"])
        .wait_with_output()
        .unwrap();
    assert_eq!(timed_out.status.code(), Some(4), "not 2, a usage error");
    assert!(
        String::from_utf8_lossy(&timed_out.stdout).contains("it is the implementer's turn"),
        "{}",
        String::from_utf8_lossy(&timed_out.stdout)
    );

    service.close_review(lani::model::Actor::Human, 1).unwrap();
    let closed = wait("reviewer", &[]).wait_with_output().unwrap();
    assert_eq!(closed.status.code(), Some(3));

    let bad = wait("reviewer", &["--timeout", "soon"])
        .wait_with_output()
        .unwrap();
    assert_eq!(bad.status.code(), Some(1), "an error, not a timeout");
}

// ── ports (§13.1) ────────────────────────────────────────────────────────────────────────────

/// A port in use for as long as the listener lives, low enough to have ports above it.
fn taken_port() -> std::net::TcpListener {
    loop {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        if listener.local_addr().unwrap().port() < 65_000 {
            return listener;
        }
    }
}

/// The stderr of a `lani` command that is expected to fail with exit code 1.
fn error_of(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    stderr
}

#[test]
fn the_server_counts_up_from_the_configured_port_past_ports_in_use() {
    let home = TestHome::new();
    let _stop = StopOnDrop(&home);
    let taken = taken_port();
    let base = taken.local_addr().unwrap().port();
    home.write_config(&format!("port = {base}\n"));

    let port = port_of(&stdout(&lani(
        &home,
        &["open", "--no-browser", "--print-url"],
    )));
    assert!(
        port > base && usize::from(port - base) < lani::port::PORT_TRIES,
        "{port} from {base}"
    );
    let log = std::fs::read_to_string(home.path.join("logs/server.log")).unwrap();
    assert!(
        log.contains(&format!("127.0.0.1:{port} (pid "))
            && log.contains(&format!("; port {base} was in use")),
        "{log}"
    );
}

#[test]
fn a_port_given_with_the_flag_must_be_free() {
    let home = TestHome::new();
    let _stop = StopOnDrop(&home);
    let taken = taken_port();
    let port = taken.local_addr().unwrap().port().to_string();
    let mut serve = common::spawn(
        Command::new(env!("CARGO_BIN_EXE_lani"))
            .args(["serve", "--port", &port])
            .envs(common::lani_env(&home.path))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped()),
    );
    // If it wrongly starts, it serves until stopped; give it ten seconds to fail.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while serve.try_wait().unwrap().is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "lani serve --port {port} started on a port in use"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let stderr = error_of(&serve.wait_with_output().unwrap());
    assert!(
        stderr.contains(&format!(
            "port {port} on 127.0.0.1 is already in use; pass another --port"
        )),
        "{stderr}"
    );
}

#[test]
fn ports_browsers_refuse_and_mistakes_in_the_config_file_are_reported() {
    let home = TestHome::new();
    let _stop = StopOnDrop(&home);
    let config = home.config_path().display().to_string();

    let flag = error_of(&lani(&home, &["serve", "--port", "6667"]));
    assert!(
        flag.contains("browsers refuse to connect to port 6667"),
        "{flag}"
    );
    home.write_config("port = 6668\n");
    for command in [
        &["open", "--no-browser"][..],
        &["serve", "--detach"],
        &["serve"],
    ] {
        let configured = error_of(&lani(&home, command));
        assert!(
            configured.contains("browsers refuse to connect to port 6668")
                && configured.contains(&format!("set another port in {config}")),
            "{command:?}: {configured}"
        );
    }
    home.write_config("prot = 7000\n");
    let typo = error_of(&lani(&home, &["open", "--no-browser"]));
    assert!(
        typo.contains(&format!("error in {config}")) && typo.contains("unknown field `prot`"),
        "{typo}"
    );
    assert!(
        !home.path.join("run/server.json").exists(),
        "no server was started"
    );
}

// ── stopping and starting around the server lock ─────────────────────────────────────────────

/// A stand-in for a LANI server: it writes `server.json` for its instance and answers the health
/// check and the shutdown request as a real server does. It doesn't take the server lock; the
/// test holds that, for as long as the process it plays would.
struct FakeServer {
    listener: std::net::TcpListener,
    health: String,
}

impl FakeServer {
    fn start(home: &std::path::Path, instance_id: &str) -> Self {
        use lani::daemon::{self, Endpoint, Health, ServerFile};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let file = ServerFile {
            pid: std::process::id(),
            endpoint: Endpoint::Tcp { port },
            token: "t".repeat(64),
            instance_id: instance_id.to_string(),
            version: daemon::VERSION.to_string(),
            started_at: "2026-10-09T12:00:00.000Z".to_string(),
        };
        file.write(home).unwrap();
        let health = serde_json::to_string(&Health {
            instance_id: file.instance_id.clone(),
            version: file.version.clone(),
            pid: file.pid,
            endpoint: file.endpoint.clone(),
            started_at: file.started_at.clone(),
            idle_timeout: "3h".to_string(),
            idle_timeout_source: "saved setting".to_string(),
            idle_remaining_secs: Some(3600),
        })
        .unwrap();
        Self { listener, health }
    }

    /// Answer requests until a shutdown request, then stop listening.
    fn serve_until_shutdown(self) {
        for stream in self.listener.incoming() {
            if self.answer(stream.unwrap()) {
                return;
            }
        }
    }

    /// Answer requests for `duration`, then stop listening.
    fn serve_for(self, duration: std::time::Duration) {
        let deadline = std::time::Instant::now() + duration;
        self.listener.set_nonblocking(true).unwrap();
        while std::time::Instant::now() < deadline {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    self.answer(stream);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => panic!("{error}"),
            }
        }
    }

    /// Answer one request; returns whether it was a shutdown request.
    fn answer(&self, mut stream: TcpStream) -> bool {
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
            request.push(byte[0]);
        }
        let shutdown = request.starts_with(b"POST /api/shutdown ");
        let body = if shutdown { "{}" } else { self.health.as_str() };
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        shutdown
    }
}

/// A stand-in for a server process that dies untidily. It holds the server lock and answers like
/// a real server, then stops listening. It keeps the lock for `linger`, as a process still
/// shutting down would, and ends without removing `server.json`, its redirect file, or a socket.
fn untidy_server(home: &TestHome, linger: std::time::Duration) -> std::thread::JoinHandle<()> {
    lani::daemon::ensure_dirs(&home.path).unwrap();
    let lock = lani::daemon::ServerLock::try_acquire(&home.path)
        .unwrap()
        .unwrap();
    let server = FakeServer::start(&home.path, "untidy");
    std::fs::write(home.path.join("run/open-untidy.html"), "token").unwrap();
    drop(std::os::unix::net::UnixListener::bind(home.path.join("run/web.sock")).unwrap());
    std::thread::spawn(move || {
        server.serve_until_shutdown();
        std::thread::sleep(linger);
        drop(lock);
    })
}

#[test]
fn stop_waits_for_the_server_process_and_clears_what_a_dead_one_left() {
    let home = TestHome::new();
    let linger = std::time::Duration::from_millis(800);
    let server = untidy_server(&home, linger);
    let started = std::time::Instant::now();
    let stopped = stdout(&lani(&home, &["stop"]));
    assert!(stopped.starts_with("Stopped the LANI server"), "{stopped}");
    assert!(
        started.elapsed() >= linger,
        "returned while the server process still held the lock"
    );
    assert!(!home.path.join("run/server.json").exists());
    assert!(!home.path.join("run/open-untidy.html").exists());
    assert!(!home.path.join("run/web.sock").exists());
    server.join().unwrap();
}

#[test]
fn stop_succeeds_when_a_new_server_takes_the_lock_first() {
    // `lani open` started a server as the old one ended, and it took the lock before `stop` could.
    // The lock is never free for `stop`, and server.json names the new server.
    let home = TestHome::new();
    lani::daemon::ensure_dirs(&home.path).unwrap();
    let lock = lani::daemon::ServerLock::try_acquire(&home.path)
        .unwrap()
        .unwrap();
    let old = FakeServer::start(&home.path, "old");
    let home_path = home.path.clone();
    let servers = std::thread::spawn(move || {
        old.serve_until_shutdown();
        let successor = FakeServer::start(&home_path, "successor");
        successor.serve_for(std::time::Duration::from_secs(2));
        drop(lock);
    });
    let stopped = stdout(&lani(&home, &["stop"]));
    assert!(stopped.starts_with("Stopped the LANI server"), "{stopped}");
    assert!(
        stopped.contains("Meanwhile another command started a new LANI server")
            && stopped.ends_with("it is still running."),
        "{stopped}"
    );
    assert!(
        home.path.join("run/server.json").exists(),
        "the new server's server.json is left alone"
    );
    servers.join().unwrap();
}

#[test]
fn open_starts_a_server_once_a_stopping_one_lets_go_of_the_lock() {
    let home = TestHome::new();
    let _stop = StopOnDrop(&home);
    // A server that has removed server.json but not yet exited still holds the lock, so the
    // server `lani open` starts finds it held and exits at once.
    lani::daemon::ensure_dirs(&home.path).unwrap();
    let lock = lani::daemon::ServerLock::try_acquire(&home.path)
        .unwrap()
        .unwrap();
    let exiting = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(1));
        drop(lock);
    });
    let url = stdout(&lani(&home, &["open", "--no-browser", "--print-url"]));
    assert!(url.contains("/login?token="), "{url}");
    exiting.join().unwrap();
}

// ── socket mode (§13.2) ──────────────────────────────────────────────────────────────────────

/// A LANI home whose configuration file puts servers in socket mode, with links for `port`.
fn socket_mode_home(port: u16) -> TestHome {
    let home = TestHome::new();
    home.write_config(&format!("listen = \"socket\"\nport = {port}\n"));
    home
}

fn server_json(home: &TestHome) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(home.path.join("run/server.json")).unwrap())
        .unwrap()
}

#[test]
fn socket_mode_serves_on_a_private_socket_that_a_forwarded_port_reaches() {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    let home = TestHome::new();
    let _stop = StopOnDrop(&home);
    let socket = home.path.join("run/web.sock");
    let host = common::machine_name();
    // Listening before the server starts, as ssh can: it connects only when a connection arrives.
    let forward = Forward::new(0, &socket);
    let port = forward.port;
    home.write_config(&format!("listen = \"socket\"\nport = {port}\n"));

    let printed = stdout(&lani(&home, &["serve", "--detach"]));
    let lines: Vec<&str> = printed.lines().collect();
    assert_eq!(
        lines[..3],
        [
            format!(
                "To reach LANI from your own machine, forward a local port to it over SSH ({host} \
                 is this machine's name; use the name you ssh to):"
            )
            .as_str(),
            &format!("ssh -L {port}:{} {host}", socket.display()),
            "To sign in, open this link; it contains the access token, so keep it private:",
        ],
        "{printed}"
    );
    let link = lines[3];
    assert!(
        link.starts_with(&format!("http://127.0.0.1:{port}/login?token="))
            && link.ends_with("&next=/"),
        "{link}"
    );

    // Only you can reach the socket, and older binaries can't read server.json.
    let metadata = std::fs::symlink_metadata(&socket).unwrap();
    assert!(metadata.file_type().is_socket());
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    let file = server_json(&home);
    assert!(file.get("port").is_none(), "{file}");
    assert_eq!(file["socket"], socket.display().to_string());
    assert_eq!(file["browser_port"], port);

    let path = link.split_once(&format!(":{port}")).unwrap().1;
    let signed_in = http_get(forward.port, path);
    assert!(signed_in.starts_with("HTTP/1.1 200"), "{signed_in}");
    assert!(
        signed_in.contains(&format!("set-cookie: lani_{port}=")),
        "{signed_in}"
    );
    let token = file["token"].as_str().unwrap();
    let cookie = format!("lani_{port}={token}");
    let page = http_get_signed_in(port, "/", &cookie);
    assert!(page.starts_with("HTTP/1.1 200"), "{page}");

    // A second forward, on another port, reaches the same server with a cookie of its own.
    let other = Forward::new(0, &socket);
    let elsewhere = http_get(other.port, path);
    assert!(
        elsewhere.contains(&format!("set-cookie: lani_{}=", other.port)),
        "{elsewhere}"
    );
    let refused = http_get_signed_in(other.port, "/", &cookie);
    assert!(refused.starts_with("HTTP/1.1 401"), "{refused}");
    let accepted = http_get_signed_in(other.port, "/", &format!("lani_{}={token}", other.port));
    assert!(accepted.starts_with("HTTP/1.1 200"), "{accepted}");

    // The other commands reach the server through its socket.
    let status = stdout(&lani(&home, &["status"]));
    assert!(
        status.contains(&format!(", socket {}, version ", socket.display()))
            && status.ends_with(lines[1]),
        "{status}"
    );
    assert_eq!(stdout(&lani(&home, &["status", "--print-url"])), link);

    // The server removes its socket as it stops, here on SIGTERM, without `lani stop`'s help.
    let pid = file["pid"].to_string();
    assert!(
        common::output(Command::new("kill").args(["-TERM", &pid]))
            .status
            .success()
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while socket.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the socket was not removed after SIGTERM"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[test]
fn open_in_socket_mode_gives_directions_and_never_starts_a_browser() {
    let port = common::LINK_PORT;
    let home = socket_mode_home(port);
    let _stop = StopOnDrop(&home);
    let (browser, browser_ran) = common::recording_browser(&home.path);
    let open = |args: &[&str]| {
        common::output(
            Command::new(env!("CARGO_BIN_EXE_lani"))
                .arg("open")
                .args(args)
                .envs(common::lani_env(&home.path))
                .env("BROWSER", &browser),
        )
    };

    let opened = open(&["--review", "3"]);
    assert!(opened.status.success(), "{opened:?}");
    assert_eq!(String::from_utf8_lossy(&opened.stdout), "");
    let directions = String::from_utf8_lossy(&opened.stderr);
    let socket = home.path.join("run/web.sock");
    assert!(
        directions.contains("LANI listens on a Unix socket, so no browser was opened.")
            && directions.contains(&format!("ssh -L {port}:{} ", socket.display()))
            && directions.contains(&format!("Then open http://127.0.0.1:{port}/reviews/3\n"))
            && directions.contains(&format!(
                "If your browser isn't signed in to this server yet, sign in first. Run lani socket \
                 on {}, which starts LANI if needed, and open the sign-in link it prints.\n",
                common::machine_name()
            )),
        "{directions}"
    );
    let token = server_json(&home)["token"].as_str().unwrap().to_string();
    assert!(!directions.contains(&token), "{directions}");

    let printed = open(&["--review", "3", "--print-url"]);
    assert!(printed.status.success(), "{printed:?}");
    assert_eq!(
        String::from_utf8_lossy(&printed.stdout).trim(),
        format!("http://127.0.0.1:{port}/login?token={token}&next=/reviews/3")
    );
    assert!(!browser_ran.exists(), "no browser was started");
}

#[test]
fn socket_starts_or_finds_a_socket_mode_server_and_says_how_to_reach_it() {
    let home = TestHome::new();
    let _stop = StopOnDrop(&home);
    let socket = home.path.join("run/web.sock");
    let port = common::LINK_PORT;

    let started = lani(&home, &["socket", "--port", &port.to_string()]);
    let printed = stdout(&started);
    assert!(
        printed.starts_with("To reach LANI from your own machine, forward a local port")
            && printed.contains(&format!("\nssh -L {port}:{} ", socket.display()))
            && printed.contains(&format!("\nhttp://127.0.0.1:{port}/login?token=")),
        "{printed}"
    );
    let told = String::from_utf8_lossy(&started.stderr);
    assert!(
        told.contains(&format!(
            "LANI server running on socket {}",
            socket.display()
        )) && told.contains("listen on a TCP port unless")
            && told.contains("sets listen = \"socket\"."),
        "without listen in the file, a later server would be on a TCP port: {told}"
    );

    let again = lani(&home, &["socket"]);
    assert_eq!(
        stdout(&again),
        printed,
        "the running server's directions and link"
    );
    // Its port can't change while it runs, and a link for the old one could reach whatever took
    // that port on your machine.
    let other_port = (port + 1).to_string();
    let moved = lani(&home, &["socket", "--port", &other_port]);
    assert_eq!(String::from_utf8_lossy(&moved.stdout), "", "no link");
    let refused = error_of(&moved);
    assert!(
        refused.contains(&format!("its command and link use port {port}"))
            && refused.contains(&format!(
                "run `lani stop`, then `lani socket --port {other_port}`"
            )),
        "{refused}"
    );
    let zero = error_of(&lani(&home, &["socket", "--port", "0"]));
    assert!(
        zero.ends_with("cannot be 0; pass another --port\n"),
        "{zero}"
    );
    let told = String::from_utf8_lossy(&again.stderr);
    assert!(told.contains("already running"), "{told}");
    home.write_config(&format!("listen = \"socket\"\nport = {port}\n"));
    let told = String::from_utf8_lossy(&lani(&home, &["socket"]).stderr).to_string();
    assert!(!told.contains("Note:"), "{told}");

    // A server on a TCP port is not passed off as one on a socket.
    stdout(&lani(&home, &["stop"]));
    home.write_config("port = 0\n");
    stdout(&lani(&home, &["open", "--no-browser"]));
    let refused = error_of(&lani(&home, &["socket"]));
    assert!(
        refused.contains("is already running on port") && refused.contains("not on a Unix socket"),
        "{refused}"
    );
}

#[test]
fn socket_refuses_a_tcp_server_that_another_command_starts_meanwhile() {
    let home = TestHome::new();
    lani::daemon::ensure_dirs(&home.path).unwrap();
    // Another command's server, still starting: it holds the lock, so the server `lani socket`
    // starts exits at once, and it answers only once `lani socket` is waiting for a server.
    let lock = lani::daemon::ServerLock::try_acquire(&home.path)
        .unwrap()
        .unwrap();
    let home_path = home.path.clone();
    let other = std::thread::spawn(move || {
        // `start_detached` creates the server's log just before starting it.
        let log = home_path.join("logs/server.log");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !log.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "lani socket never started a server"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        FakeServer::start(&home_path, "tcp").serve_for(std::time::Duration::from_secs(2));
        drop(lock);
    });
    let refused = error_of(&lani(
        &home,
        &["socket", "--port", &common::LINK_PORT.to_string()],
    ));
    assert!(
        refused.contains("is already running on port") && refused.contains("not on a Unix socket"),
        "{refused}"
    );
    other.join().unwrap();
}

#[test]
fn a_dead_servers_socket_is_replaced_but_nothing_else_is() {
    let home = socket_mode_home(lani::port::DEFAULT_PORT.get());
    let _stop = StopOnDrop(&home);
    stdout(&lani(&home, &["serve", "--detach"]));
    let pid = server_json(&home)["pid"].to_string();
    assert!(
        common::output(Command::new("kill").args(["-KILL", &pid]))
            .status
            .success()
    );
    let socket = home.path.join("run/web.sock");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while lani::daemon::ServerLock::try_acquire(&home.path)
        .unwrap()
        .is_none()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the killed server kept its lock"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(socket.exists(), "a killed server leaves its socket behind");

    let restarted = stdout(&lani(&home, &["serve", "--detach"]));
    assert!(restarted.contains("/login?token="), "{restarted}");
    let stopped = stdout(&lani(&home, &["stop"]));
    assert!(stopped.starts_with("Stopped the LANI server"), "{stopped}");

    std::fs::write(&socket, "notes").unwrap();
    let refused = error_of(&lani(&home, &["open", "--no-browser"]));
    assert!(
        refused.contains(&format!(
            "cannot listen on {}: something other than a socket is there",
            socket.display()
        )),
        "{refused}"
    );
    assert_eq!(std::fs::read_to_string(&socket).unwrap(), "notes");
}

#[test]
fn a_socket_path_too_long_for_the_os_is_refused() {
    use std::os::unix::fs::DirBuilderExt;
    let home = TestHome::new();
    home.write_config("port = 5264\n");
    // Too long for any temporary directory: the limit is 103 bytes on macOS and 107 on Linux.
    let long_home = home.path.join("h".repeat(110));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&long_home)
        .unwrap();
    let _stop = StopInHome(&home, &long_home);
    let socket = long_home.join("run/web.sock");
    for command in [
        &["serve", "--listen", "socket"][..],
        &["serve", "--detach", "--listen", "socket"],
    ] {
        let mut child = common::spawn(
            Command::new(env!("CARGO_BIN_EXE_lani"))
                .args(command)
                .envs(common::lani_env(&home.path))
                .env("LANI_HOME", &long_home)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        );
        // A server that wrongly starts in the foreground serves until stopped; give it ten seconds
        // to fail.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                panic!("{command:?} started a server on a socket path too long for the OS");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let refused = error_of(&child.wait_with_output().unwrap());
        assert!(
            refused.contains(&format!(
                "the socket path {} is {} bytes, too long for a Unix socket",
                socket.display(),
                socket.as_os_str().len()
            )),
            "{command:?}: {refused}"
        );
    }
}

/// Stops the server of a LANI home other than the test's own, here `.1`, using the test's
/// configuration file, even if an assertion fails part-way.
struct StopInHome<'a>(&'a TestHome, &'a std::path::Path);

impl Drop for StopInHome<'_> {
    fn drop(&mut self) {
        let _ = common::output(
            Command::new(env!("CARGO_BIN_EXE_lani"))
                .arg("stop")
                .envs(common::lani_env(&self.0.path))
                .env("LANI_HOME", self.1),
        );
    }
}
