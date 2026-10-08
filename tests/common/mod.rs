//! Shared helpers for integration tests: throwaway git repositories and LANI homes.

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers"
)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;

/// Serializes process spawning within a test binary.
///
/// On macOS a pipe created by one thread can be inherited by a child that another thread forks
/// before the pipe is marked close-on-exec. If that child starts a long-lived LANI server, the
/// server keeps the other thread's pipe open and that thread waits for EOF forever. Spawning one
/// process at a time closes the window; the processes still run concurrently.
static SPAWN: Mutex<()> = Mutex::new(());

/// Spawn `command` while holding the spawn lock.
pub fn spawn(command: &mut Command) -> Child {
    let _guard = SPAWN.lock().unwrap_or_else(PoisonError::into_inner);
    command.spawn().expect("spawn process")
}

/// Run `command` to completion, capturing stdout and stderr (spawning under the spawn lock).
pub fn output(command: &mut Command) -> Output {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    spawn(command).wait_with_output().expect("wait for process")
}

/// A git repository in a temporary directory, with a `main` branch.
pub struct TestRepo {
    _dir: TempDir,
    pub path: PathBuf,
}

impl TestRepo {
    /// A repository whose `main` branch has one commit containing `files`.
    pub fn new(files: &[(&str, &str)]) -> Self {
        Self::create(None, files)
    }

    /// Like [`TestRepo::new`], but in a directory with the given name.
    pub fn named(name: &str, files: &[(&str, &str)]) -> Self {
        Self::create(Some(name), files)
    }

    fn create(name: Option<&str>, files: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().expect("create temp dir");
        // Canonicalize so paths match what git reports (macOS /var → /private/var).
        let mut path = std::fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        if let Some(name) = name {
            path.push(name);
            std::fs::create_dir(&path).expect("create named repo dir");
        }
        let repo = Self { _dir: dir, path };
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["config", "user.name", "Test"]);
        repo.git(&["config", "user.email", "test@example.com"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        for (file, content) in files {
            repo.write(file, content);
        }
        repo.commit_all("initial");
        repo
    }

    /// Run git in the repository and return its trimmed stdout; panics on failure.
    pub fn git(&self, args: &[&str]) -> String {
        let output = output(
            Command::new("git")
                .arg("-C")
                .arg(&self.path)
                .args(args)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE"),
        );
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("utf-8")
            .trim()
            .to_string()
    }

    pub fn write(&self, file: &str, content: &str) {
        let path = self.path.join(file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(path, content).expect("write file");
    }

    pub fn commit_all(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
        self.head()
    }

    pub fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    pub fn switch_new(&self, branch: &str) {
        self.git(&["switch", "-q", "-c", branch]);
    }

    pub fn switch(&self, branch: &str) {
        self.git(&["switch", "-q", branch]);
    }

    pub fn index_bytes(&self) -> Vec<u8> {
        std::fs::read(self.path.join(".git/index")).expect("read index")
    }
}

/// A private LANI data directory.
pub struct TestHome {
    _dir: TempDir,
    pub path: PathBuf,
}

impl TestHome {
    /// A data directory with mode 0700, as LANI requires.
    pub fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = std::fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("make temp dir private");
        Self { _dir: dir, path }
    }

    pub fn db_path(&self) -> PathBuf {
        self.path.join("lani.db")
    }
}

/// Lines `1`..=`count` of the form "line N", newline-terminated.
pub fn numbered(count: u32) -> String {
    (1..=count).fold(String::new(), |mut text, n| {
        text.push_str("line ");
        text.push_str(&n.to_string());
        text.push('\n');
        text
    })
}

pub fn path_of(repo: &TestRepo) -> &Path {
    &repo.path
}

// ── MCP client ───────────────────────────────────────────────────────────────────────────────

const MCP_TIMEOUT: Duration = Duration::from_secs(20);

/// A minimal MCP client speaking newline-delimited JSON-RPC to a `lani mcp` child process.
pub struct McpClient {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
}

impl McpClient {
    pub fn start(home: &Path) -> Self {
        Self::start_with_env(home, &[])
    }

    pub fn start_with_env(home: &Path, env: &[(&str, &str)]) -> Self {
        Self::start_in(home, &std::env::current_dir().expect("test cwd"), env)
    }

    /// Start `lani mcp` with `cwd` as its working directory, as Claude Code starts it in the
    /// session's directory.
    pub fn start_in(home: &Path, cwd: &Path, env: &[(&str, &str)]) -> Self {
        Self::start_program(Path::new(env!("CARGO_BIN_EXE_lani")), home, cwd, env)
    }

    /// Start `program mcp`, where `program` is a lani binary.
    pub fn start_program(program: &Path, home: &Path, cwd: &Path, env: &[(&str, &str)]) -> Self {
        let mut child = spawn(
            Command::new(program)
                .arg("mcp")
                .current_dir(cwd)
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

    /// The `lani mcp` process's id.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn send(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").expect("write to lani mcp");
        self.stdin.flush().expect("flush");
    }

    /// Send a request and return the full JSON-RPC response with the matching id.
    pub fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let mut message = json!({"jsonrpc": "2.0", "id": id, "method": method});
        message["params"] = params;
        self.send(&message);
        loop {
            let line = match self.lines.recv_timeout(MCP_TIMEOUT) {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => {
                    panic!("no response to {method} within {MCP_TIMEOUT:?}")
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
    pub fn call(&mut self, tool: &str, arguments: Value) -> (bool, String) {
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

    pub fn ok(&mut self, tool: &str, arguments: Value) -> String {
        let (is_error, text) = self.call(tool, arguments);
        assert!(!is_error, "{tool} failed: {text}");
        text
    }

    pub fn err(&mut self, tool: &str, arguments: Value) -> String {
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
