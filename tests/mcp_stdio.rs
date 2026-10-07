//! End-to-end tests: drive the real `lani mcp` binary over stdio, as Claude Code does.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::Duration;

use common::{TestHome, TestRepo, numbered};
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(20);

/// A minimal MCP client speaking newline-delimited JSON-RPC to a `lani mcp` child process.
struct McpClient {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
}

impl McpClient {
    fn start(home: &Path) -> Self {
        Self::start_with_env(home, &[])
    }

    fn start_with_env(home: &Path, env: &[(&str, &str)]) -> Self {
        let mut child = common::spawn(
            Command::new(env!("CARGO_BIN_EXE_lani"))
                .arg("mcp")
                .env("LANI_HOME", home)
                .envs(env.iter().copied())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit()),
        );
        let stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");
        // Read on a thread so a misbehaving server fails the test instead of hanging it.
        let (sender, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let mut client = Self {
            child,
            stdin,
            lines,
            next_id: 1,
        };
        let init = client.request(
            "initialize",
            json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "lani-test", "version": "0"}
            }),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "lani", "{init}");
        client.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        client
    }

    fn send(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").expect("write to lani mcp");
        self.stdin.flush().expect("flush");
    }

    /// Send a request and return the full JSON-RPC response with the matching id.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let mut message = json!({"jsonrpc": "2.0", "id": id, "method": method});
        message["params"] = params;
        self.send(&message);
        loop {
            let line = match self.lines.recv_timeout(TIMEOUT) {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => {
                    panic!("no response to {method} within {TIMEOUT:?}")
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("lani mcp exited before answering {method}")
                }
            };
            let message: Value = serde_json::from_str(&line).expect("server sent JSON");
            if message["id"] == json!(id) {
                return message;
            }
        }
    }

    /// Call a tool; returns whether it reported an error, and its text.
    fn call(&mut self, tool: &str, arguments: Value) -> (bool, String) {
        let mut params = json!({"name": tool});
        params["arguments"] = arguments;
        let response = self.request("tools/call", params);
        let result = &response["result"];
        assert!(
            result.is_object(),
            "{tool} returned a protocol error: {response}"
        );
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        (result["isError"] == json!(true), text)
    }

    fn ok(&mut self, tool: &str, arguments: Value) -> String {
        let (is_error, text) = self.call(tool, arguments);
        assert!(!is_error, "{tool} failed: {text}");
        text
    }

    fn err(&mut self, tool: &str, arguments: Value) -> String {
        let (is_error, text) = self.call(tool, arguments);
        assert!(is_error, "{tool} should have failed but returned: {text}");
        text
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn feature_repo() -> TestRepo {
    let repo = TestRepo::new(&[("src/lib.rs", &numbered(20))]);
    repo.switch_new("feature");
    repo.write(
        "src/lib.rs",
        &numbered(20).replace("line 5\n", "line five\n"),
    );
    repo.commit_all("feature work");
    repo
}

#[test]
fn initialize_advertises_instructions_and_tools() {
    let home = TestHome::new();
    let mut client = McpClient::start(&home.path);

    let response = client.request("tools/list", json!({}));
    let tools = response["result"]["tools"].as_array().expect("tool list");
    let mut names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "add_comment",
            "close_review",
            "create_issue",
            "edit_comment",
            "get_issue",
            "get_review",
            "list_issues",
            "list_reviews",
            "open_web_ui",
            "set_issue_status",
            "start_review",
            "update_review_summary",
        ]
    );
    let create = tools.iter().find(|t| t["name"] == "create_issue").unwrap();
    let required = create["inputSchema"]["required"].as_array().unwrap();
    assert!(required.contains(&json!("worktree_path")), "{create}");
    let list = tools.iter().find(|t| t["name"] == "list_issues").unwrap();
    assert_eq!(list["annotations"]["readOnlyHint"], json!(true), "{list}");
}

#[test]
fn reviewer_and_implementer_sessions_complete_a_review_loop() {
    let home = TestHome::new();
    let repo = feature_repo();
    let repo_path = repo.path.to_str().unwrap();
    let mut reviewer = McpClient::start(&home.path);

    let started = reviewer.ok(
        "start_review",
        json!({"repo_path": repo_path, "title": "Rename line five", "author": "reviewer",
               "base_mode": "auto", "summary": "Spells out line 5."}),
    );
    assert!(
        started.starts_with("Created review 1 \"Rename line five\""),
        "{started}"
    );
    assert!(
        started.contains("base main (auto: nearest parent branch, 1 commit)"),
        "{started}"
    );
    assert!(started.contains("no uncommitted changes"), "{started}");

    let head = repo.head();
    let created = reviewer.ok(
        "create_issue",
        json!({"review_id": 1, "author": "reviewer", "body": "Use a digit here.\nConsistency.",
               "path": "src/lib.rs", "side": "head", "start_line": 5, "end_line": 5,
               "start_line_text": "line five", "commit": head, "worktree_path": repo_path}),
    );
    assert!(
        created.starts_with("Created issue 1 in review 1."),
        "{created}"
    );

    // The reviewer session cannot switch roles.
    let locked = reviewer.err(
        "add_comment",
        json!({"author": "implementer", "body": "x", "issue_id": 1}),
    );
    assert!(locked.contains("acting as the reviewer"), "{locked}");

    // A separate implementer session reads only open issues, fixes, and replies.
    let mut implementer = McpClient::start(&home.path);
    let reviews = implementer.ok(
        "list_reviews",
        json!({"status": "open", "repo_path": repo_path}),
    );
    assert!(reviews.contains("Review 1 · open"), "{reviews}");
    assert!(reviews.contains("← branch checked out here"), "{reviews}");
    let issues = implementer.ok(
        "list_issues",
        json!({"review_id": 1, "status": "open", "detail": "full"}),
    );
    assert!(
        issues.contains("Issue 1 · open · review 1 · opened by reviewer"),
        "{issues}"
    );
    assert!(issues.contains("> 5 | line five"), "{issues}");

    repo.write("src/lib.rs", &numbered(20));
    let fix = repo.commit_all("use a digit");
    let reply = implementer.ok(
        "add_comment",
        json!({"author": "implementer", "body": format!("Fixed in {fix}."), "issue_id": 1}),
    );
    assert!(reply.starts_with("Added comment c2 to issue 1."), "{reply}");
    let forbidden = implementer.err(
        "set_issue_status",
        json!({"issue_id": 1, "author": "implementer", "status": "resolved"}),
    );
    assert!(
        forbidden.contains("implementers cannot resolve"),
        "{forbidden}"
    );

    // The reviewer sees the fix as an outdated anchor plus the reply, and resolves.
    let issue = reviewer.ok("get_issue", json!({"issue_id": 1}));
    assert!(issue.contains("OUTDATED"), "{issue}");
    assert!(issue.contains("[c2 v1] implementer"), "{issue}");
    let resolved = reviewer.ok(
        "set_issue_status",
        json!({"issue_id": 1, "author": "reviewer", "status": "resolved", "comment": "Verified."}),
    );
    assert!(resolved.starts_with("Resolved issue 1"), "{resolved}");
    let open = reviewer.ok(
        "list_issues",
        json!({"review_id": 1, "status": "open", "detail": "summary"}),
    );
    assert!(
        open.contains("showing 0 open · not shown: 1 resolved"),
        "{open}"
    );

    let review = implementer.ok("get_review", json!({"review_id": 1}));
    assert!(
        review.contains("## Summary (v1 · last edited by reviewer"),
        "{review}"
    );
    assert!(review.contains("Spells out line 5."), "{review}");
}

#[test]
fn invalid_arguments_are_reported() {
    let home = TestHome::new();
    let mut client = McpClient::start(&home.path);
    let both = client.err(
        "add_comment",
        json!({"author": "reviewer", "body": "x", "issue_id": 1, "review_id": 1}),
    );
    assert!(
        both.contains("exactly one of issue_id and review_id"),
        "{both}"
    );
    let base = client.err(
        "start_review",
        json!({"repo_path": "/", "title": "t", "author": "reviewer", "base_mode": "ref"}),
    );
    assert!(base.contains("needs base_ref"), "{base}");

    // A missing required field (worktree_path) is rejected before the tool runs.
    let response = client.request(
        "tools/call",
        json!({"name": "create_issue", "arguments": {
            "review_id": 1, "author": "reviewer", "body": "b", "path": "a", "side": "head",
            "start_line": 1, "end_line": 1, "start_line_text": "x", "commit": "HEAD"}}),
    );
    let rejected = response["error"].is_object() || response["result"]["isError"] == json!(true);
    assert!(
        rejected,
        "missing worktree_path must be rejected: {response}"
    );
}

#[test]
fn open_web_ui_starts_the_server_and_opens_the_browser() {
    let home = TestHome::new();
    let repo = feature_repo();
    // `true` stands in for the browser: it accepts the redirect file and exits successfully.
    let mut client = McpClient::start_with_env(&home.path, &[("BROWSER", "true")]);
    client.ok(
        "start_review",
        json!({"repo_path": repo.path.to_str().unwrap(), "title": "t", "author": "reviewer",
               "base_mode": "auto"}),
    );
    let missing = client.err("open_web_ui", json!({"review_id": 99}));
    assert!(missing.contains("review 99 does not exist"), "{missing}");

    let opened = client.ok("open_web_ui", json!({"review_id": 1}));
    let stop = common::output(
        Command::new(env!("CARGO_BIN_EXE_lani"))
            .arg("stop")
            .env("LANI_HOME", &home.path),
    );
    assert!(opened.starts_with("Opened http://127.0.0.1:"), "{opened}");
    assert!(
        opened.contains("/reviews/1 in the human's browser."),
        "{opened}"
    );
    assert!(opened.contains("Started the LANI web server."), "{opened}");
    assert!(
        !opened.contains("token"),
        "the token must not reach Claude: {opened}"
    );
    assert!(
        String::from_utf8_lossy(&stop.stdout).starts_with("Stopped"),
        "server was running"
    );
}
