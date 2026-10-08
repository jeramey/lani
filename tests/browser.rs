//! Browser tests: line selection and the new-issue form in a real browser, driven through W3C
//! WebDriver (docs/design.md §19). They need a browser and its driver, so they are opt-in:
//!
//! ```text
//! cargo test --test browser -- --ignored
//! ```
//!
//! They drive Safari through `safaridriver`, which ships with macOS. Enable it once:
//! Safari → Settings → Advanced → "Show features for web developers", then Develop →
//! "Allow Remote Automation", and run `safaridriver --enable`. Safari opens a separate automation
//! window for each test; leave it alone until it closes.
//!
//! The WebDriver client is plain HTTP and JSON over `std`, so the tests add no dependencies.

mod common;

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use common::{TestHome, TestRepo, numbered};
use lani::base::BaseMode;
use lani::model::{Actor, IssueStatus, Severity, Side};
use lani::service::{CommentTarget, CreateIssue, Service, SpanRequest, StartReview};
use serde_json::{Value, json};

/// Safari runs one automation session at a time, so the tests take turns.
static BROWSER: Mutex<()> = Mutex::new(());

/// The key under which WebDriver returns an element reference.
const ELEMENT: &str = "element-6066-11e4-a52e-4f735466cecf";
/// How long to wait for a page to reach an expected state.
const TIMEOUT: Duration = Duration::from_secs(15);
const SAFARIDRIVER: &str = "/usr/bin/safaridriver";

/// `src/lib.rs` in hex: the id of its section in the Changes view.
const LIB_ANCHOR: &str = "file-7372632f6c69622e7273";

#[test]
#[ignore = "needs Safari with remote automation enabled; run with --ignored"]
fn selecting_lines_in_the_file_view_opens_an_issue_at_the_rendered_commit() {
    let _turn = BROWSER.lock().unwrap_or_else(PoisonError::into_inner);
    let fixture = Fixture::new();
    let browser = WebDriver::safari();
    let file = format!("/reviews/{}/files/src/lib.rs", fixture.review_id);
    browser.sign_in(&fixture, &file);
    browser.watch_for_problems();

    browser.click("tr[data-line=\"10\"] td.ln a");
    browser.shift_click("tr[data-line=\"12\"] td.ln a");
    assert_eq!(browser.text("return location.hash;"), "#L10-L12");
    assert_eq!(
        browser.selected_lines(),
        "10,11,12",
        "shift-click selects the range"
    );
    assert_eq!(
        browser.text("return document.querySelector('tr.new-issue-bar .range').textContent;"),
        "lines 10–12"
    );

    browser.click("tr.new-issue-bar button");
    let fields = browser.form_fields();
    assert_eq!(fields["path"], "src/lib.rs");
    assert_eq!(fields["side"], "head");
    assert_eq!(
        (&fields["start_line"], &fields["end_line"]),
        (&json!("10"), &json!("12"))
    );
    assert_eq!(
        fields["commit"],
        fixture.head.as_str(),
        "the rendered commit"
    );
    assert_eq!(fields["merge_base"], "");
    assert_eq!(fields["back"], file.as_str());
    assert_eq!(
        browser.run(
            "return document.activeElement === document.querySelector('form.new-issue input[name=\"title\"]');"
        ),
        true,
        "the title has focus"
    );
    browser.fill_title_and_severity("Lines 10 to 12 need work", "low");

    browser.type_text(
        "form.new-issue textarea",
        "**Bold** from the browser\n\n<script>alert(1)</script>",
    );
    browser.click("form.new-issue .editor-tabs button[data-tab=\"preview\"]");
    browser.wait_for(
        "return document.querySelector('form.new-issue .preview').innerHTML.includes('<strong>');",
        "the preview",
    );
    let preview =
        browser.text("return document.querySelector('form.new-issue .preview').innerHTML;");
    assert!(preview.contains("<strong>Bold</strong>"), "{preview}");
    assert!(
        !preview.contains("<script"),
        "raw HTML is escaped: {preview}"
    );
    browser.click("form.new-issue .editor-tabs button[data-tab=\"write\"]");
    assert_eq!(
        browser.run("return document.querySelector('form.new-issue textarea').hidden;"),
        false
    );
    browser.assert_no_problems("selecting and previewing");

    browser.click("form.new-issue button[type=\"submit\"]");
    let issue_id = browser.wait_for_new_issue(&file);
    assert_eq!(
        browser.row_above_thread(issue_id),
        "L12",
        "the thread follows its last line"
    );
    let view = fixture.service().get_issue(issue_id).unwrap();
    assert_eq!(view.issue.author, Actor::Human);
    assert_eq!(view.issue.title, "Lines 10 to 12 need work");
    assert_eq!(view.issue.severity, Severity::Low);
    assert_eq!(view.issue.anchors[0].side, Side::Head);
    assert_eq!(
        view.issue.anchors[0].commit, fixture.head,
        "anchored to the rendered commit"
    );
    assert_eq!(
        (
            view.issue.anchors[0].start_line,
            view.issue.anchors[0].end_line
        ),
        (10, 12)
    );
    assert!(
        view.comments[0].body.contains("<script>alert(1)</script>"),
        "stored as written"
    );

    // A link to a selection restores it, here by changing only the hash.
    browser.navigate(&format!("{}{file}#L20-L21", fixture.base));
    browser.wait_for(
        "return document.querySelectorAll('tr.selected').length === 2;",
        "the linked selection",
    );
    assert_eq!(browser.selected_lines(), "20,21");
}

#[test]
#[ignore = "needs Safari with remote automation enabled; run with --ignored"]
fn removed_lines_in_the_changes_view_open_base_side_issues_and_threads_return_there() {
    let _turn = BROWSER.lock().unwrap_or_else(PoisonError::into_inner);
    let fixture = Fixture::new();
    let browser = WebDriver::safari();
    let changes = format!("/reviews/{}/changes", fixture.review_id);
    browser.sign_in(&fixture, &changes);
    browser.watch_for_problems();

    let removed_row = format!("{LIB_ANCHOR}-B20");
    browser.click(&format!("#{removed_row} td.ln a"));
    assert_eq!(
        browser.text("return location.hash;"),
        format!("#{removed_row}")
    );
    assert_eq!(
        browser.text("return document.querySelector('tr.new-issue-bar .range').textContent;"),
        "line 20 (base side)"
    );
    browser.click("tr.new-issue-bar button");
    let fields = browser.form_fields();
    assert_eq!(
        fields["side"], "base",
        "a removed line anchors to the base side"
    );
    assert_eq!(
        (&fields["start_line"], &fields["end_line"]),
        (&json!("20"), &json!("20"))
    );
    assert_eq!(fields["path"], "src/lib.rs");
    assert_eq!(fields["commit"], fixture.head.as_str());
    assert_eq!(
        fields["merge_base"],
        fixture.merge_base.as_str(),
        "the merge-base shown"
    );
    assert_eq!(fields["back"], changes.as_str());
    browser.fill_title_and_severity("Line 20 is gone", "nit");
    browser.type_text("form.new-issue textarea", "Why was line 20 removed?");
    browser.click("form.new-issue button[type=\"submit\"]");
    let issue_id = browser.wait_for_new_issue(&changes);
    assert_eq!(browser.row_above_thread(issue_id), removed_row);
    let view = fixture.service().get_issue(issue_id).unwrap();
    assert_eq!(view.issue.anchors[0].side, Side::Base);
    assert_eq!(view.issue.title, "Line 20 is gone");
    assert_eq!(view.issue.severity, Severity::Nit);
    assert_eq!(view.issue.anchors[0].commit, fixture.merge_base);
    assert_eq!(
        (
            view.issue.anchors[0].start_line,
            view.issue.anchors[0].end_line
        ),
        (20, 20)
    );

    reply_and_resolve_in_place(&browser, &fixture, &changes, issue_id);

    // Cancel removes the form.
    browser.watch_for_problems();
    browser.click(&format!("#{LIB_ANCHOR}-B5 td.ln a"));
    browser.click("tr.new-issue-bar button");
    browser.click("form.new-issue .cancel");
    assert_eq!(
        browser.run("return !!document.querySelector('form.new-issue');"),
        false
    );
    browser.assert_no_problems("the Changes view");
}

#[test]
#[ignore = "needs Safari with remote automation enabled; run with --ignored"]
fn live_updates_refresh_the_page_but_never_an_open_editor() {
    let _turn = BROWSER.lock().unwrap_or_else(PoisonError::into_inner);
    let fixture = Fixture::new();
    let browser = WebDriver::safari();
    let review_id = fixture.review_id;
    let overview = format!("/reviews/{review_id}");
    browser.sign_in(&fixture, &overview);
    browser.watch_for_problems();
    // Gone if the page reloads: updates must arrive in place.
    browser.run("window.laniSamePage = true; return true;");
    let in_place = "window.laniSamePage === true";

    // Another session comments and hands the review on.
    let mut service = fixture.service();
    service
        .add_comment(
            Actor::Reviewer,
            CommentTarget::Summary(review_id),
            "From another session.",
        )
        .unwrap();
    service
        .hand_off(Actor::Reviewer, review_id, Some(Actor::Implementer), None)
        .unwrap();
    browser.wait_for(
        &format!(
            "return {in_place} \
             && document.querySelector('#conversation').textContent.includes('From another session.') \
             && document.querySelector('#review-head .turn').textContent.includes('Claude · implementer');"
        ),
        "the comment and the turn, in place",
    );

    // A refresh that fails says so, and the next one that succeeds takes the notice away.
    let refresh_failed = "[...document.querySelectorAll('.live-notice')]\
                          .some(notice => notice.textContent.includes('could not be updated'))";
    browser.run(
        "window.laniFetch = window.fetch; \
         window.fetch = () => Promise.reject(new Error('offline')); return true;",
    );
    service
        .add_comment(
            Actor::Implementer,
            CommentTarget::Summary(review_id),
            "While offline.",
        )
        .unwrap();
    browser.wait_for(&format!("return {refresh_failed};"), "the failed refresh");
    browser.run("window.fetch = window.laniFetch; return true;");
    service
        .add_comment(
            Actor::Implementer,
            CommentTarget::Summary(review_id),
            "Back online.",
        )
        .unwrap();
    browser.wait_for(
        &format!(
            "return {in_place} && !({refresh_failed}) \
             && document.querySelector('#conversation').textContent.includes('Back online.');"
        ),
        "the next refresh, without the notice",
    );

    // A draft in progress is never replaced; its part says it is out of date instead.
    browser.type_text("#new-summary-comment", "My draft");
    service
        .add_comment(
            Actor::Implementer,
            CommentTarget::Summary(review_id),
            "A second comment.",
        )
        .unwrap();
    browser.wait_for(
        &format!("return {in_place} && !!document.querySelector('#conversation > .stale-notice');"),
        "the out-of-date notice",
    );
    assert_eq!(
        browser.run(
            "return [document.querySelector('#new-summary-comment').value, \
             document.querySelector('#conversation').textContent.includes('A second comment.')];"
        ),
        json!(["My draft", false]),
        "the draft is kept, and the part is not replaced"
    );

    // Parts nobody is editing in still change: closing the review from another session swaps
    // the toolbar's forms for "Reopen review".
    service.close_review(Actor::Reviewer, review_id).unwrap();
    browser.wait_for(
        &format!(
            "const toolbar = document.querySelector('#toolbar'); \
             return {in_place} && toolbar.textContent.includes('Reopen review') \
             && !toolbar.querySelector('.stale-notice');"
        ),
        "the closed review's toolbar, in place",
    );
    service.reopen_review(Actor::Human, review_id).unwrap();
    browser.wait_for(
        "return document.querySelector('#toolbar').textContent.includes('Close review');",
        "the reopened review's toolbar",
    );

    // In a code view, a selection survives its table being replaced.
    let file = format!("/reviews/{review_id}/files/src/lib.rs");
    browser.navigate(&format!("{}{file}", fixture.base));
    browser.wait_for(
        &format!(
            "return location.pathname === {} && document.readyState === 'complete';",
            json!(file)
        ),
        "the file view",
    );
    browser.watch_for_problems();
    browser.click("tr[data-line=\"10\"] td.ln a");
    browser.shift_click("tr[data-line=\"12\"] td.ln a");
    let issue = service
        .create_issue(&CreateIssue {
            review_id,
            author: Actor::Reviewer,
            title: "Line 30",
            severity: Severity::Low,
            body: "Filed by another session.",
            spans: vec![SpanRequest {
                path: "src/lib.rs",
                side: Side::Head,
                start_line: 30,
                end_line: 30,
                // The branch removed line 20.
                start_line_text: Some("line 31"),
            }],
            commit: &fixture.head,
            worktree_path: None,
            read_merge_base: None,
        })
        .unwrap();
    browser.wait_for(
        &format!(
            "return !!document.querySelector('#issue-{}');",
            issue.issue.id
        ),
        "the new thread",
    );
    assert_eq!(browser.selected_lines(), "10,11,12");
    assert_eq!(
        browser.run("return !!document.querySelector('tr.new-issue-bar');"),
        true,
        "the selection's bar is back"
    );
    browser.assert_no_problems("live updates");

    // When the server stops, the page says so.
    let stopped = lani(&fixture.home.path, &["stop"]);
    assert!(stopped.status.success());
    browser.wait_for(
        "return [...document.querySelectorAll('.live-notice')]\
           .some(notice => notice.textContent.startsWith('LANI was stopped.'));",
        "the stop notice",
    );
}

/// Reply to and resolve an issue from its inline thread; both return to the same view.
fn reply_and_resolve_in_place(browser: &WebDriver, fixture: &Fixture, page: &str, issue_id: i64) {
    let thread = format!("#issue-{issue_id}");
    browser.type_text(
        &format!("{thread} form.new-comment textarea"),
        "A reply from the code.",
    );
    browser.click(&format!(
        "{thread} form.new-comment button[type=\"submit\"]"
    ));
    browser.wait_for(
        &format!(
            "return location.pathname === {} && location.hash.startsWith('#c') && document.readyState === 'complete';",
            json!(page)
        ),
        "the reply",
    );
    assert_eq!(
        browser.run("return document.querySelector(location.hash).textContent.includes('A reply from the code.');"),
        true
    );
    browser.click(&format!(
        "{thread} form.thread-status button[type=\"submit\"]"
    ));
    browser.wait_for(
        &format!("return location.hash === '{thread}' && document.readyState === 'complete';"),
        "the resolve",
    );
    assert_eq!(
        browser.run(&format!(
            "return !document.querySelector('{thread} details').open \
             && document.querySelector('{thread} summary .badge.resolved').textContent.startsWith('Resolved');"
        )),
        true,
        "the resolved thread is collapsed"
    );
    let view = fixture.service().get_issue(issue_id).unwrap();
    assert_eq!(view.issue.status, IssueStatus::Resolved);
    assert_eq!(view.issue.resolved_by, Some(Actor::Human));
    assert_eq!(view.comments.len(), 2);
}

// ── fixture ──────────────────────────────────────────────────────────────────────────────────

/// A diff review of `feature` against `main`, served by the real `lani` binary. On `feature`,
/// line 5 of `src/lib.rs` is rewritten and line 20 removed.
struct Fixture {
    home: TestHome,
    _repo: TestRepo,
    review_id: i64,
    /// `http://127.0.0.1:<port>`
    base: String,
    token: String,
    head: String,
    merge_base: String,
}

impl Fixture {
    fn new() -> Self {
        let repo = TestRepo::new(&[("src/lib.rs", &numbered(40))]);
        let merge_base = repo.head();
        repo.switch_new("feature");
        repo.write(
            "src/lib.rs",
            &numbered(40)
                .replace("line 5\n", "line five\n")
                .replace("line 20\n", ""),
        );
        let head = repo.commit_all("feature work");
        let home = TestHome::new();
        let review_id = Service::open(&home.db_path())
            .unwrap()
            .start_review(&StartReview {
                repo_path: &repo.path,
                title: "Browser test",
                author: Actor::Reviewer,
                base_mode: BaseMode::Ref("main".into()),
                summary: None,
                head_ref: None,
            })
            .unwrap()
            .view
            .review
            .id;
        let output = lani(&home.path, &["open", "--no-browser", "--print-url"]);
        assert!(
            output.status.success(),
            "lani open failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        // http://127.0.0.1:<port>/login?token=<token>&next=/
        let url = String::from_utf8(output.stdout).unwrap();
        let (base, query) = url.trim().split_once("/login?token=").unwrap();
        let token = query.split('&').next().unwrap();
        Self {
            base: base.to_string(),
            token: token.to_string(),
            home,
            _repo: repo,
            review_id,
            head,
            merge_base,
        }
    }

    fn service(&self) -> Service {
        Service::open(&self.home.db_path()).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = lani(&self.home.path, &["stop"]);
    }
}

fn lani(home: &Path, args: &[&str]) -> std::process::Output {
    common::output(
        Command::new(env!("CARGO_BIN_EXE_lani"))
            .args(args)
            .env("LANI_HOME", home),
    )
}

// ── WebDriver ────────────────────────────────────────────────────────────────────────────────

/// A browser session through a WebDriver server. Dropping it ends the session, which closes the
/// browser window, and stops the driver.
struct WebDriver {
    port: u16,
    driver: Child,
    session: String,
}

impl WebDriver {
    fn safari() -> Self {
        assert!(
            Path::new(SAFARIDRIVER).exists(),
            "these tests drive Safari through {SAFARIDRIVER}, which only macOS has"
        );
        let port = TcpListener::bind(("127.0.0.1", 0))
            .and_then(|listener| listener.local_addr())
            .unwrap()
            .port();
        let mut driver = common::spawn(
            Command::new(SAFARIDRIVER)
                .args(["-p", &port.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        );
        let deadline = Instant::now() + TIMEOUT;
        while http(port, "GET", "/status", None).is_err() {
            assert!(Instant::now() < deadline, "safaridriver did not start");
            std::thread::sleep(Duration::from_millis(50));
        }
        let capabilities =
            json!({ "capabilities": { "alwaysMatch": { "browserName": "safari" } } });
        let (status, reply) = http(port, "POST", "/session", Some(&capabilities)).unwrap();
        if status != 200 {
            let _ = driver.kill();
            panic!(
                "could not start a Safari session: {}\nEnable it once: Safari → Settings → Advanced → \
                 \"Show features for web developers\", Develop → \"Allow Remote Automation\", then \
                 `safaridriver --enable`.",
                reply["value"]["message"]
            );
        }
        Self {
            port,
            driver,
            session: reply["value"]["sessionId"].as_str().unwrap().to_string(),
        }
    }

    /// Send a command to this session, returning its `value`, or the WebDriver error.
    fn try_command(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
        let path = format!("/session/{}{path}", self.session);
        match http(self.port, method, &path, body) {
            Ok((200, reply)) => Ok(reply["value"].clone()),
            Ok((status, reply)) => Err(format!("{method} {path}: {status} {}", reply["value"])),
            Err(error) => Err(format!("{method} {path}: {error}")),
        }
    }

    fn command(&self, method: &str, path: &str, body: Option<&Value>) -> Value {
        self.try_command(method, path, body)
            .unwrap_or_else(|error| panic!("WebDriver command failed: {error}"))
    }

    fn navigate(&self, url: &str) {
        self.command("POST", "/url", Some(&json!({ "url": url })));
    }

    /// Sign in with the server's token, continuing to `next` (a path without `#` or `&`).
    fn sign_in(&self, fixture: &Fixture, next: &str) {
        self.navigate(&format!(
            "{}/login?token={}&next={next}",
            fixture.base, fixture.token
        ));
        // The sign-in page continues with a meta refresh, which keeps the SameSite=Strict cookie.
        self.wait_for(
            &format!(
                "return location.pathname === {} && document.readyState === 'complete';",
                json!(next)
            ),
            "the signed-in page",
        );
    }

    /// Run `script` (a function body) in the page and return its result.
    fn run(&self, script: &str) -> Value {
        self.command(
            "POST",
            "/execute/sync",
            Some(&json!({ "script": script, "args": [] })),
        )
    }

    fn text(&self, script: &str) -> String {
        match self.run(script) {
            Value::String(text) => text,
            other => panic!("expected text from {script:?}, got {other}"),
        }
    }

    /// Wait until `script` returns `true`. Errors while a page loads count as "not yet".
    fn wait_for(&self, script: &str, what: &str) {
        let deadline = Instant::now() + TIMEOUT;
        let body = json!({ "script": script, "args": [] });
        loop {
            if self.try_command("POST", "/execute/sync", Some(&body)) == Ok(Value::Bool(true)) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn find(&self, selector: &str) -> String {
        let found = self.command(
            "POST",
            "/element",
            Some(&json!({ "using": "css selector", "value": selector })),
        );
        found[ELEMENT].as_str().unwrap().to_string()
    }

    /// Scroll `selector` into view and wait until it stops moving. Safari keeps scrolling for a
    /// moment after the page focuses a field, and a click whose position is computed meanwhile
    /// lands nowhere: no event reaches the page, although WebDriver reports success.
    fn scroll_to(&self, selector: &str) {
        let script = format!(
            "document.querySelector({}).scrollIntoView({{ block: 'center' }}); return true;",
            json!(selector)
        );
        self.run(&script);
        let position = format!(
            "const r = document.querySelector({}).getBoundingClientRect(); \
             return r.left + ',' + r.top + ',' + window.scrollY;",
            json!(selector)
        );
        let deadline = Instant::now() + TIMEOUT;
        let mut last = self.text(&position);
        loop {
            std::thread::sleep(Duration::from_millis(50));
            let now = self.text(&position);
            if now == last {
                return;
            }
            assert!(Instant::now() < deadline, "{selector} kept moving");
            last = now;
        }
    }

    fn click(&self, selector: &str) {
        self.scroll_to(selector);
        let element = self.find(selector);
        self.command(
            "POST",
            &format!("/element/{element}/click"),
            Some(&json!({})),
        );
    }

    /// Click with Shift held on the keyboard, as a person extends a selection.
    fn shift_click(&self, selector: &str) {
        self.scroll_to(selector);
        let element = self.find(selector);
        let shift = "\u{E008}";
        let pause = json!({ "type": "pause", "duration": 0 });
        let actions = json!({ "actions": [
            { "type": "key", "id": "keyboard", "actions": [
                { "type": "keyDown", "value": shift }, pause, pause, pause,
                { "type": "keyUp", "value": shift } ] },
            { "type": "pointer", "id": "mouse", "parameters": { "pointerType": "mouse" }, "actions": [
                pause, { "type": "pointerMove", "origin": { ELEMENT: element }, "x": 0, "y": 0 },
                { "type": "pointerDown", "button": 0 }, { "type": "pointerUp", "button": 0 }, pause ] },
        ] });
        self.command("POST", "/actions", Some(&actions));
        self.command("DELETE", "/actions", None);
    }

    /// Type into a field with the keyboard.
    fn type_text(&self, selector: &str, text: &str) {
        self.scroll_to(selector);
        let element = self.find(selector);
        self.command(
            "POST",
            &format!("/element/{element}/value"),
            Some(&json!({ "text": text })),
        );
    }

    /// Type the new-issue form's title and pick its severity, as a person would.
    fn fill_title_and_severity(&self, title: &str, severity: &str) {
        self.type_text("form.new-issue input[name=\"title\"]", title);
        self.click(&format!(
            "form.new-issue select[name=\"severity\"] option[value=\"{severity}\"]"
        ));
        let chosen =
            "return document.querySelector('form.new-issue select[name=\"severity\"]').value;";
        assert_eq!(self.text(chosen), severity);
    }

    fn selected_lines(&self) -> String {
        self.text("return [...document.querySelectorAll('tr.selected')].map(row => row.dataset.line).join();")
    }

    /// The named fields of the open new-issue form.
    fn form_fields(&self) -> Value {
        self.run(
            "return Object.fromEntries([...document.querySelector('form.new-issue').elements]\
             .filter(field => field.name).map(field => [field.name, field.value]));",
        )
    }

    /// After a submit: wait to return to `page` at a new thread, and return the issue's id.
    fn wait_for_new_issue(&self, page: &str) -> i64 {
        self.wait_for(
            &format!(
                "return location.pathname === {} && location.hash.startsWith('#issue-') \
                 && document.readyState === 'complete';",
                json!(page)
            ),
            "the new issue",
        );
        self.text("return location.hash;")["#issue-".len()..]
            .parse()
            .unwrap()
    }

    /// The id of the code row that an issue's thread follows.
    fn row_above_thread(&self, issue_id: i64) -> String {
        self.text(&format!(
            "let row = document.querySelector('#issue-{issue_id}').closest('tr').previousElementSibling;\
             while (row && !row.dataset.line) row = row.previousElementSibling;\
             return row.id;"
        ))
    }

    /// From now on, record Content-Security-Policy violations and script errors on this page.
    fn watch_for_problems(&self) {
        self.run(
            "window.laniProblems = [];\
             document.addEventListener('securitypolicyviolation', event =>\
               window.laniProblems.push('CSP ' + event.violatedDirective + ' ' + event.blockedURI));\
             window.addEventListener('error', event => window.laniProblems.push('error ' + event.message));\
             return true;",
        );
    }

    fn assert_no_problems(&self, during: &str) {
        let problems =
            self.run("return window.laniProblems || ['the page reloaded since watching began'];");
        assert_eq!(problems, json!([]), "problems during {during}");
    }
}

impl Drop for WebDriver {
    fn drop(&mut self) {
        let _ = self.try_command("DELETE", "", None);
        let _ = self.driver.kill();
        let _ = self.driver.wait();
    }
}

/// One HTTP/1.1 request to the WebDriver server: the status code and the JSON body.
fn http(port: u16, method: &str, path: &str, body: Option<&Value>) -> io::Result<(u16, Value)> {
    let body = body.map(Value::to_string).unwrap_or_default();
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
         Content-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )?;
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    let status = status_line
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| io::Error::other(format!("unexpected status line {status_line:?}")))?;
    let (mut length, mut chunked) = (None, false);
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-length") {
                length = value.parse::<usize>().ok();
            } else if name.eq_ignore_ascii_case("transfer-encoding") {
                chunked = value.eq_ignore_ascii_case("chunked");
            }
        }
    }
    let mut content = Vec::new();
    if chunked {
        loop {
            let mut size = String::new();
            reader.read_line(&mut size)?;
            let size = size.trim().split(';').next().unwrap_or_default();
            let size = usize::from_str_radix(size, 16).map_err(io::Error::other)?;
            if size == 0 {
                break;
            }
            let start = content.len();
            content.resize(start + size, 0);
            reader.read_exact(&mut content[start..])?;
            reader.read_exact(&mut [0; 2])?;
        }
    } else if let Some(length) = length {
        content.resize(length, 0);
        reader.read_exact(&mut content)?;
    } else {
        reader.read_to_end(&mut content)?;
    }
    let json = if content.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&content).map_err(io::Error::other)?
    };
    Ok((status, json))
}
