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

    let stopped = stdout(&lani(&home, &["stop"]));
    assert!(stopped.starts_with("Stopped the LANI server"), "{stopped}");
    assert!(!server_file.exists(), "server.json is removed on shutdown");
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
    assert!(response.starts_with("HTTP/1.1 303"), "{response}");
    assert!(
        response.contains(&format!("set-cookie: lani_{port}=")),
        "{response}"
    );
    assert!(response.contains("location: /issues/7"), "{response}");

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
