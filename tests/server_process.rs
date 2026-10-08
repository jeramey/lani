//! Process tests: the real `lani` binary starting, finding, and stopping its web server.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Command, Output, Stdio};

use common::TestHome;

fn lani(home: &TestHome, args: &[&str]) -> Output {
    common::output(
        Command::new(env!("CARGO_BIN_EXE_lani"))
            .args(args)
            .env("LANI_HOME", &home.path),
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
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        stream,
        "GET {path_and_query} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
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
                    .env("LANI_HOME", &home.path)
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
            .env("LANI_HOME", &home.path),
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
            .env("LANI_HOME", &home.path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        common::spawn(&mut command)
    };

    // Not the implementer's turn yet: the wait blocks until the reviewer hands off.
    let waiting = wait("implementer", &[]);
    std::thread::sleep(std::time::Duration::from_millis(1500));
    service
        .hand_off(
            lani::model::Actor::Reviewer,
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
