//! Benchmarks against the performance budget (docs/design.md §16). They drive the real `lani`
//! binary the way Claude Code and the browser do. Opt-in, and meaningful only in a release build:
//!
//! ```text
//! cargo test --release --test bench -- --ignored --nocapture
//! ```
//!
//! The report shows every measurement next to its target, and the test fails if a target is
//! missed. Rows without a target measure costs that §21 defers until benchmarks show they matter.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::{McpClient, TestHome, TestRepo, numbered};
use lani::daemon::ServerFile;
use serde_json::json;

/// Repetitions of each timing.
const RUNS: usize = 40;
/// Reviews started to time parent detection, each on a new branch.
const NEW_BRANCHES: usize = 20;
/// Other local branches parent detection measures the distance to.
const OTHER_BRANCHES: usize = 20;
/// Fresh servers started to time a first render, which includes loading the syntax set.
const FIRST_RENDERS: usize = 5;
/// How long a process is watched to measure its idle CPU use.
const IDLE_WINDOW: Duration = Duration::from_secs(10);
/// The most CPU an idle process may use, in percent of one core: "~0" in §16.
const IDLE_CPU_PERCENT: f64 = 1.0;

#[test]
#[ignore = "benchmarks: cargo test --release --test bench -- --ignored --nocapture"]
fn performance_budget() {
    #[allow(
        clippy::assertions_on_constants,
        reason = "the build profile is a constant, and it is what this check is about"
    )]
    {
        assert!(
            !cfg!(debug_assertions),
            "benchmarks need a release build: cargo test --release --test bench -- --ignored \
             --nocapture"
        );
    }
    let home = TestHome::new();
    let repo = bench_repo();
    let mut rows = mcp_rows(&home, &repo);
    rows.extend(start_review_rows(&home, &repo));
    rows.extend(web_rows(&home, &repo));
    rows.push(binary_size_row());
    report(&rows);
}

// ── MCP ──────────────────────────────────────────────────────────────────────────────────────

fn mcp_rows(home: &TestHome, repo: &TestRepo) -> Vec<Row> {
    let mut rows = Vec::new();
    let started = Samples::collect(RUNS, |_| {
        let start = Instant::now();
        let client = McpClient::start(&home.path);
        let elapsed = start.elapsed();
        drop(client);
        elapsed
    });
    rows.push(started.row("lani mcp start → ready", Some((Stat::Median, ms(20)))));

    let mut client = McpClient::start(&home.path);
    let worktree = repo.path.to_str().unwrap();
    client.ok(
        "start_review",
        json!({"repo_path": worktree, "title": "Benchmark", "author": "reviewer",
               "base_mode": "auto", "summary": "Measures LANI."}),
    );

    // Tools that run no git.
    let added = client.ok(
        "add_comment",
        json!({"author": "reviewer", "review_id": 1, "body": "A comment to edit."}),
    );
    let comment_id = comment_id(&added);
    let edited = Samples::collect(RUNS, |run| {
        let arguments = json!({"comment_id": comment_id, "author": "reviewer",
                               "body": format!("Edit {run}."), "expected_version": run + 1});
        time(|| client.ok("edit_comment", arguments))
    });
    rows.push(edited.row(
        "MCP tool call, no git: edit_comment",
        Some((Stat::P95, ms(10))),
    ));
    let listed = Samples::collect(RUNS, |_| {
        time(|| client.ok("list_reviews", json!({"status": "all"})))
    });
    rows.push(listed.row(
        "MCP tool call, no git: list_reviews",
        Some((Stat::P95, ms(10))),
    ));

    // One issue per run, each anchor checked against the commit and the file on disk.
    let head = repo.head();
    let created = Samples::collect(RUNS, |run| {
        let line = 10 + run;
        let arguments = json!({"review_id": 1, "author": "reviewer",
            "title": format!("Finding {run}"), "severity": "low",
            "body": "A **finding**, with `code` and a list:\n\n- one\n- two",
            "spans": [{"path": "src/lib.rs", "side": "head", "start_line": line,
                       "end_line": line, "start_line_text": format!("line {line}")}],
            "commit": head, "worktree_path": worktree});
        time(|| client.ok("create_issue", arguments))
    });
    rows.push(created.row(
        "MCP tool call with anchor validation: create_issue",
        Some((Stat::P95, ms(50))),
    ));

    let reviewed = Samples::collect(RUNS, |_| {
        time(|| client.ok("get_review", json!({"review_id": 1})))
    });
    rows.push(reviewed.row("get_review, with git state", None));
    let placed = Samples::collect(RUNS, |_| {
        time(|| {
            client.ok(
                "list_issues",
                json!({"review_id": 1, "status": "all", "detail": "full"}),
            )
        })
    });
    rows.push(placed.row(
        &format!("list_issues full: {RUNS} issues placed, with code (§21 placement cache)"),
        None,
    ));

    rows.push(rss_row("RSS: lani mcp", client.pid(), 15));
    rows.push(idle_cpu_row("Idle CPU: lani mcp", client.pid(), true));
    rows
}

fn start_review_rows(home: &TestHome, repo: &TestRepo) -> Vec<Row> {
    let mut client = McpClient::start(&home.path);
    let worktree = repo.path.to_str().unwrap();
    let started = Samples::collect(NEW_BRANCHES, |run| {
        repo.git(&["switch", "-q", "-c", &format!("bench-{run}"), "main"]);
        repo.write("bench.txt", &format!("{run}\n"));
        repo.commit_all("bench");
        time(|| {
            client.ok(
                "start_review",
                json!({"repo_path": worktree, "title": format!("Branch {run}"),
                       "author": "reviewer", "base_mode": "auto"}),
            )
        })
    });
    repo.switch("feature");
    vec![started.row(
        &format!("start_review with parent detection, {OTHER_BRANCHES}+ branches"),
        Some((Stat::Median, ms(100))),
    )]
}

// ── web ──────────────────────────────────────────────────────────────────────────────────────

fn web_rows(home: &TestHome, repo: &TestRepo) -> Vec<Row> {
    let mut rows = Vec::new();
    // The first render on a fresh server loads the syntax set and highlights the file.
    let file = "/reviews/1/files/src/big.rs";
    let first = Samples::collect(FIRST_RENDERS, |_| {
        let fresh = Server::open(home);
        time(|| fresh.get(file))
    });
    rows.push(first.row(
        "First render of a 2,000-line file",
        Some((Stat::Median, ms(250))),
    ));

    let server = Server::open(home);
    let reopened = Samples::collect(RUNS, |_| {
        time(|| lani(&home.path, &["open", "--no-browser", "--print-url"]))
    });
    rows.push(reopened.row(
        "lani open, server running (--no-browser)",
        Some((Stat::Median, ms(100))),
    ));
    server.get(file);
    let cached = Samples::collect(RUNS, |_| time(|| server.get(file)));
    rows.push(cached.row(
        "Cached page render (that file)",
        Some((Stat::Median, ms(30))),
    ));
    let overview = Samples::collect(RUNS, |_| time(|| server.get("/reviews/1?status=all")));
    rows.push(overview.row(
        &format!("Overview with {RUNS} issues, Markdown rendered (§21 Markdown cache)"),
        None,
    ));
    let changes = Samples::collect(RUNS, |_| time(|| server.get("/reviews/1/changes")));
    rows.push(changes.row(&format!("Changes view with {RUNS} issues placed"), None));
    rows.push(rss_row("RSS: lani serve", server.pid, 100));

    rows.push(idle_cpu_row("Idle CPU: lani serve", server.pid, true));
    let stream = server.open_stream("/reviews/1/events");
    rows.push(idle_cpu_row(
        "CPU: lani serve with one live-update stream",
        server.pid,
        false,
    ));
    drop(stream);
    let parsed = Samples::collect(RUNS, |_| {
        time(|| repo.git(&["rev-parse", "--verify", "refs/heads/feature"]))
    });
    rows.push(parsed.row("git rev-parse, one ref (§21 watching refs)", None));
    rows
}

/// A running `lani serve`, reached with its token as a bearer credential. Dropping it stops
/// the server.
struct Server {
    home: std::path::PathBuf,
    port: u16,
    token: String,
    pid: u32,
}

impl Server {
    /// `lani open --no-browser --print-url`, which starts a server unless one is running.
    fn open(home: &TestHome) -> Self {
        let output = lani(&home.path, &["open", "--no-browser", "--print-url"]);
        assert!(
            output.status.success(),
            "lani open failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        // http://127.0.0.1:<port>/login?token=<token>&next=/
        let url = String::from_utf8(output.stdout).unwrap();
        let (base, query) = url.trim().split_once("/login?token=").unwrap();
        let server = ServerFile::read(&home.path).unwrap();
        Self {
            home: home.path.clone(),
            port: base.rsplit(':').next().unwrap().parse().unwrap(),
            token: query.split('&').next().unwrap().to_string(),
            pid: server.pid,
        }
    }

    fn request(&self, path: &str) -> TcpStream {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\n\
             Connection: close\r\n\r\n",
            self.port, self.token
        )
        .unwrap();
        stream
    }

    /// GET `path` and read the whole response, which must be a 200.
    fn get(&self, path: &str) {
        let mut response = Vec::new();
        self.request(path).read_to_end(&mut response).unwrap();
        assert!(
            response.starts_with(b"HTTP/1.1 200 "),
            "GET {path}: {}",
            String::from_utf8_lossy(&response[..response.len().min(300)])
        );
    }

    /// Open a live-update stream and leave it open until the returned socket is dropped.
    fn open_stream(&self, path: &str) -> TcpStream {
        let mut stream = self.request(path);
        let mut head = [0; 12];
        stream.read_exact(&mut head).unwrap();
        assert_eq!(&head, b"HTTP/1.1 200");
        stream
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        assert!(lani(&self.home, &["stop"]).status.success());
    }
}

// ── other rows ───────────────────────────────────────────────────────────────────────────────

fn binary_size_row() -> Row {
    let bytes = std::fs::metadata(env!("CARGO_BIN_EXE_lani")).unwrap().len();
    let mib = mebibytes(bytes / 1024);
    Row {
        metric: "Release binary size".to_string(),
        measured: format!("{mib:.1} MiB"),
        target: "< 25 MiB".to_string(),
        within: Some(mib < 25.0),
    }
}

/// A process's resident memory.
fn rss_row(metric: &str, pid: u32, limit_mib: u32) -> Row {
    let output = command_output(Command::new("ps").args(["-o", "rss=", "-p", &pid.to_string()]));
    let kib: u64 = output.trim().parse().unwrap();
    let mib = mebibytes(kib);
    Row {
        metric: metric.to_string(),
        measured: format!("{mib:.1} MiB"),
        target: format!("< {limit_mib} MiB"),
        within: Some(mib < f64::from(limit_mib)),
    }
}

/// CPU used by an idle process over [`IDLE_WINDOW`], in percent of one core. `capped` rows must
/// stay under [`IDLE_CPU_PERCENT`]; the others are measured only.
fn idle_cpu_row(metric: &str, pid: u32, capped: bool) -> Row {
    let before = cpu_seconds(pid);
    std::thread::sleep(IDLE_WINDOW);
    let used = cpu_seconds(pid) - before;
    let percent = used / IDLE_WINDOW.as_secs_f64() * 100.0;
    Row {
        metric: metric.to_string(),
        measured: format!(
            "{percent:.2}% ({:.0} ms CPU in {}s)",
            used * 1000.0,
            IDLE_WINDOW.as_secs()
        ),
        target: if capped {
            format!("~0 (< {IDLE_CPU_PERCENT}%)")
        } else {
            String::new()
        },
        within: capped.then_some(percent < IDLE_CPU_PERCENT),
    }
}

/// A process's total CPU time, from `ps -o time=`: `[[dd-]hh:]mm:ss[.ff]`.
fn cpu_seconds(pid: u32) -> f64 {
    let output = command_output(Command::new("ps").args(["-o", "time=", "-p", &pid.to_string()]));
    let (days, clock) = match output.trim().split_once('-') {
        Some((days, clock)) => (days.parse::<f64>().unwrap(), clock.to_string()),
        None => (0.0, output.trim().to_string()),
    };
    let seconds = clock.split(':').fold(0.0, |total, part| {
        total * 60.0 + part.parse::<f64>().unwrap()
    });
    days * 86_400.0 + seconds
}

// ── setup ────────────────────────────────────────────────────────────────────────────────────

/// `main` with `src/lib.rs` and a 2,000-line `src/big.rs`, other local branches for parent
/// detection to measure, and `feature` checked out, which changes `src/lib.rs`.
fn bench_repo() -> TestRepo {
    let mut big = String::new();
    for line in include_str!("../src/service/mod.rs")
        .lines()
        .cycle()
        .take(2000)
    {
        big.push_str(line);
        big.push('\n');
    }
    let repo = TestRepo::new(&[("src/lib.rs", &numbered(400)), ("src/big.rs", &big)]);
    for branch in 0..OTHER_BRANCHES {
        repo.git(&["switch", "-q", "-c", &format!("other-{branch}"), "main"]);
        repo.write("other.txt", &format!("{branch}\n"));
        repo.commit_all("other work");
    }
    repo.git(&["switch", "-q", "-c", "feature", "main"]);
    repo.write(
        "src/lib.rs",
        &numbered(400).replace("line 5\n", "line five\n"),
    );
    repo.commit_all("feature work");
    repo
}

fn lani(home: &Path, args: &[&str]) -> std::process::Output {
    common::output(
        Command::new(env!("CARGO_BIN_EXE_lani"))
            .args(args)
            .env("LANI_HOME", home),
    )
}

fn command_output(command: &mut Command) -> String {
    let output = common::output(command);
    assert!(output.status.success(), "{command:?} failed");
    String::from_utf8(output.stdout).unwrap()
}

/// The id in "Added comment c12 …".
fn comment_id(added: &str) -> i64 {
    added
        .split_whitespace()
        .find_map(|word| word.strip_prefix('c')?.parse().ok())
        .unwrap_or_else(|| panic!("no comment id in {added:?}"))
}

// ── measuring and reporting ──────────────────────────────────────────────────────────────────

/// How long `operation` takes.
fn time<T>(operation: impl FnOnce() -> T) -> Duration {
    let start = Instant::now();
    operation();
    start.elapsed()
}

fn ms(millis: u64) -> Duration {
    Duration::from_millis(millis)
}

fn mebibytes(kib: u64) -> f64 {
    f64::from(u32::try_from(kib).unwrap()) / 1024.0
}

#[derive(Clone, Copy)]
enum Stat {
    Median,
    P95,
}

/// Timings, sorted.
struct Samples(Vec<Duration>);

impl Samples {
    /// Run `run` with each run's index; it returns the time to count, leaving setup out.
    fn collect(runs: usize, mut run: impl FnMut(usize) -> Duration) -> Self {
        let mut samples: Vec<Duration> = (0..runs).map(&mut run).collect();
        samples.sort_unstable();
        Self(samples)
    }

    fn median(&self) -> Duration {
        self.0[self.0.len() / 2]
    }

    fn p95(&self) -> Duration {
        self.0[(self.0.len() * 95).div_ceil(100) - 1]
    }

    /// A row with the median and p95, held to `target` if there is one.
    fn row(&self, metric: &str, target: Option<(Stat, Duration)>) -> Row {
        let millis = |duration: Duration| duration.as_secs_f64() * 1000.0;
        let (target_text, within) = match target {
            None => (String::new(), None),
            Some((stat, limit)) => {
                let (name, value) = match stat {
                    Stat::Median => ("median", self.median()),
                    Stat::P95 => ("p95", self.p95()),
                };
                (
                    format!("< {} ms {name}", limit.as_millis()),
                    Some(value < limit),
                )
            }
        };
        Row {
            metric: metric.to_string(),
            measured: format!(
                "median {:.1} ms, p95 {:.1} ms (n={})",
                millis(self.median()),
                millis(self.p95()),
                self.0.len()
            ),
            target: target_text,
            within,
        }
    }
}

struct Row {
    metric: String,
    measured: String,
    /// Empty for rows that are measured only.
    target: String,
    /// `None` for rows that are measured only.
    within: Option<bool>,
}

fn report(rows: &[Row]) {
    println!();
    println!(
        "{:<66} {:<40} {:<22} result",
        "metric", "measured", "target"
    );
    for row in rows {
        let result = match row.within {
            Some(true) => "ok",
            Some(false) => "OVER",
            None => "-",
        };
        println!(
            "{:<66} {:<40} {:<22} {result}",
            row.metric, row.measured, row.target
        );
    }
    let over: Vec<&str> = rows
        .iter()
        .filter(|row| row.within == Some(false))
        .map(|row| row.metric.as_str())
        .collect();
    assert!(over.is_empty(), "over budget: {}", over.join(", "));
}
