//! Web UI tests: requests go through the real router and middleware in-process.

mod common;

use std::time::Duration;

use axum::Router;
use axum::body::{Body, BodyDataStream};
use axum::http::{Request, StatusCode, header};
use common::{TestHome, TestRepo, numbered};
use futures_util::StreamExt as _;
use lani::base::BaseMode;
use lani::executable::Executable;
use lani::model::{Actor, IssueStatus, Severity, Side};
use lani::service::{CommentTarget, CreateIssue, Service, SpanRequest, StartReview};
use lani::settings::IdleTimeout;
use lani::web::{self, AppState, Instance, Stop, TimeoutSource};
use tower::ServiceExt as _;

const PORT: u16 = 4242;
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const HOST: &str = "127.0.0.1:4242";
const ORIGIN: &str = "http://127.0.0.1:4242";

struct Fixture {
    home: TestHome,
    repo: TestRepo,
    state: AppState,
    app: Router,
    review_id: i64,
    issue_id: i64,
    /// The reviewer's comment that opened the issue.
    issue_comment_id: i64,
}

/// A review of `feature` with one issue and one summary comment, served by a router.
fn fixture() -> Fixture {
    fixture_with(None)
}

/// Like [`fixture`], for a server started from `executable`.
fn fixture_with(executable: Option<Executable>) -> Fixture {
    let home = TestHome::new();
    let repo = TestRepo::new(&[("src/lib.rs", &numbered(20))]);
    repo.switch_new("feature");
    repo.write(
        "src/lib.rs",
        &numbered(20).replace("line 5\n", "line five\n"),
    );
    repo.commit_all("feature work");

    let mut service = Service::open(&home.db_path()).unwrap();
    let review_id = service
        .start_review(&StartReview {
            repo_path: &repo.path,
            title: "Spell out five",
            author: Actor::Reviewer,
            base_mode: BaseMode::Auto,
            summary: Some("Makes line 5 **readable**."),
            head_ref: None,
        })
        .unwrap()
        .view
        .review
        .id;
    let head = repo.head();
    let issue = service
        .create_issue(&CreateIssue {
            review_id,
            author: Actor::Reviewer,
            title: "A problem",
            severity: Severity::Medium,
            body: "Use a digit.\n\n<script>alert('pwned')</script>",
            spans: vec![SpanRequest {
                path: "src/lib.rs",
                side: Side::Head,
                start_line: 5,
                end_line: 5,
                start_line_text: Some("line five"),
            }],
            commit: &head,
            worktree_path: Some(&repo.path),
            read_merge_base: None,
        })
        .unwrap();
    service
        .add_comment(
            Actor::Implementer,
            CommentTarget::Summary(review_id),
            "Thanks for looking.",
        )
        .unwrap();

    let state = AppState::new(
        service,
        Instance {
            token: TOKEN.to_string(),
            instance_id: "test-instance".to_string(),
            port: PORT,
            pid: 1,
            started_at: "2026-10-07T12:00:00.000Z".to_string(),
            home: home.path.clone(),
        },
        executable,
        (
            IdleTimeout::After(Duration::from_secs(3 * 3600)),
            TimeoutSource::Saved,
        ),
    );
    Fixture {
        app: web::router(state.clone()),
        state,
        review_id,
        issue_id: issue.issue.id,
        issue_comment_id: issue.comments[0].id,
        home,
        repo,
    }
}

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .uri(path)
        .header(header::HOST, HOST)
        .header(header::COOKIE, format!("lani_{PORT}={TOKEN}"))
        .body(Body::empty())
        .unwrap()
}

fn post(path: &str, form: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::HOST, HOST)
        .header(header::COOKIE, format!("lani_{PORT}={TOKEN}"))
        .header(header::ORIGIN, ORIGIN)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form.to_string()))
        .unwrap()
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, headers, String::from_utf8(body.to_vec()).unwrap())
}

fn location(headers: &axum::http::HeaderMap) -> &str {
    headers.get(header::LOCATION).unwrap().to_str().unwrap()
}

fn encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                char::from(byte).to_string()
            }
            b' ' => "+".to_string(),
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

// ── authentication and browser security ──────────────────────────────────────────────────────

#[tokio::test]
async fn requests_need_the_token_and_the_right_host() {
    let f = fixture();
    let anonymous = Request::builder()
        .uri("/")
        .header(header::HOST, HOST)
        .body(Body::empty())
        .unwrap();
    let (status, headers, body) = send(&f.app, anonymous).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.contains("lani open"), "{body}");
    assert!(
        headers.contains_key(header::CONTENT_SECURITY_POLICY),
        "headers on rejections too"
    );

    let wrong_cookie = Request::builder()
        .uri("/")
        .header(header::HOST, HOST)
        .header(header::COOKIE, format!("lani_{PORT}=nope"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&f.app, wrong_cookie).await.0, StatusCode::UNAUTHORIZED);

    let rebinding = Request::builder()
        .uri("/")
        .header(header::HOST, "evil.example:4242")
        .header(header::COOKIE, format!("lani_{PORT}={TOKEN}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(&f.app, rebinding).await.0,
        StatusCode::MISDIRECTED_REQUEST
    );

    let bearer = Request::builder()
        .uri("/api/health")
        .header(header::HOST, "localhost:4242")
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = send(&f.app, bearer).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"instance_id\":\"test-instance\""), "{body}");
}

#[tokio::test]
async fn login_sets_a_strict_port_scoped_cookie_and_continues_only_locally() {
    let f = fixture();
    let login = |query: &str| {
        Request::builder()
            .uri(format!("/login?{query}"))
            .header(header::HOST, HOST)
            .body(Body::empty())
            .unwrap()
    };
    // A page, not a redirect: Strict cookies are withheld on redirects in a navigation that
    // started cross-site (from the local redirect file), but sent on the page's own navigation.
    let (status, headers, body) =
        send(&f.app, login(&format!("token={TOKEN}&next=/reviews/1"))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key(header::LOCATION));
    let cookie = headers.get(header::SET_COOKIE).unwrap().to_str().unwrap();
    assert_eq!(
        cookie,
        format!("lani_{PORT}={TOKEN}; HttpOnly; SameSite=Strict; Path=/")
    );
    assert!(
        body.contains(r#"<meta http-equiv="refresh" content="0;url=/reviews/1">"#),
        "{body}"
    );
    assert!(
        body.contains(r#"href="/reviews/1""#),
        "a manual fallback link: {body}"
    );

    for hostile in [
        "//evil.example",
        "https://evil.example",
        "/x%22%3E%3Cscript%3E",
        "/x;url=//evil.example",
    ] {
        let (_, _, body) = send(&f.app, login(&format!("token={TOKEN}&next={hostile}"))).await;
        assert!(
            body.contains(r#"content="0;url=/""#),
            "{hostile} is replaced by /: {body}"
        );
    }

    let (status, headers, _) = send(&f.app, login("token=wrong&next=/")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(!headers.contains_key(header::SET_COOKIE));
}

#[tokio::test]
async fn state_changes_need_a_same_origin_request_unless_using_the_bearer_token() {
    let f = fixture();
    let url = format!("/reviews/{}/comments", f.review_id);
    let mut cross_site = post(&url, "body=hi");
    cross_site
        .headers_mut()
        .insert(header::ORIGIN, "http://evil.example".parse().unwrap());
    assert_eq!(send(&f.app, cross_site).await.0, StatusCode::FORBIDDEN);

    let mut no_origin = post(&url, "body=hi");
    no_origin.headers_mut().remove(header::ORIGIN);
    assert_eq!(send(&f.app, no_origin).await.0, StatusCode::FORBIDDEN);

    // What a browser may send for a form post: `Origin: null`. Sec-Fetch-Site decides.
    let mut null_origin_cross_site = post(&url, "body=hi");
    null_origin_cross_site
        .headers_mut()
        .insert(header::ORIGIN, "null".parse().unwrap());
    null_origin_cross_site
        .headers_mut()
        .insert("sec-fetch-site", "cross-site".parse().unwrap());
    assert_eq!(
        send(&f.app, null_origin_cross_site).await.0,
        StatusCode::FORBIDDEN
    );
    let mut null_origin_same_origin = post(&url, "body=hi");
    null_origin_same_origin
        .headers_mut()
        .insert(header::ORIGIN, "null".parse().unwrap());
    null_origin_same_origin
        .headers_mut()
        .insert("sec-fetch-site", "same-origin".parse().unwrap());
    assert_eq!(
        send(&f.app, null_origin_same_origin).await.0,
        StatusCode::SEE_OTHER
    );

    let mut fetch_metadata = post(&url, "body=hi");
    fetch_metadata.headers_mut().remove(header::ORIGIN);
    fetch_metadata
        .headers_mut()
        .insert("sec-fetch-site", "same-origin".parse().unwrap());
    assert_eq!(send(&f.app, fetch_metadata).await.0, StatusCode::SEE_OTHER);

    let shutdown = Request::builder()
        .method("POST")
        .uri("/api/shutdown")
        .header(header::HOST, HOST)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&f.app, shutdown).await.0, StatusCode::OK);
    tokio::time::timeout(Duration::from_secs(1), f.state.shutdown_requested())
        .await
        .expect("shutdown was requested");
}

#[tokio::test]
async fn health_checks_do_not_count_as_activity() {
    let f = fixture();
    let before = f.state.idle().last_activity;
    tokio::time::sleep(Duration::from_millis(5)).await;
    let health = Request::builder()
        .uri("/api/health")
        .header(header::HOST, HOST)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    send(&f.app, health).await;
    assert_eq!(f.state.idle().last_activity, before);
    send(&f.app, get("/")).await;
    assert!(f.state.idle().last_activity > before);
}

// ── pages ────────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn pages_render_with_security_headers() {
    let f = fixture();
    let (status, headers, body) = send(&f.app, get("/")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Spell out five"), "{body}");
    assert!(body.contains("feature → main"), "{body}");
    let csp = headers
        .get(header::CONTENT_SECURITY_POLICY)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        csp.contains("script-src 'self'") && csp.contains("img-src 'self' data:"),
        "{csp}"
    );
    assert_eq!(headers.get(header::REFERRER_POLICY).unwrap(), "same-origin");
    assert_eq!(
        headers.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
        "nosniff"
    );

    let (status, _, body) = send(&f.app, get(&format!("/reviews/{}", f.review_id))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("<strong>readable</strong>"),
        "summary is rendered Markdown"
    );
    assert!(
        body.contains("Thanks for looking."),
        "summary comments are shown"
    );
    assert!(
        body.contains("Claude · implementer"),
        "authors are labelled"
    );
    assert!(body.contains("auto-detected"), "the base's origin is shown");
    assert!(
        body.contains("started the review"),
        "the activity timeline is shown"
    );

    let (status, _, body) = send(
        &f.app,
        get(&format!("/reviews/{}/issues/{}", f.review_id, f.issue_id)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("line five"), "the code excerpt is shown");
    assert!(
        body.contains("class=\"hl\""),
        "the anchored line is highlighted"
    );

    let (status, headers, _) = send(&f.app, get(&format!("/issues/{}", f.issue_id))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        location(&headers),
        format!("/reviews/{}/issues/{}", f.review_id, f.issue_id)
    );

    let (status, _, body) = send(&f.app, get("/settings")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("3h (saved setting)"), "{body}");

    let (status, headers, _) = send(&f.app, get("/static/lani.css")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/css")
    );
    let (status, headers, css) = send(&f.app, get("/static/highlight.css")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/css")
    );
    assert!(
        css.contains(".hl-comment"),
        "{}",
        &css[..css.len().min(300)]
    );

    assert_eq!(send(&f.app, get("/nowhere")).await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        send(&f.app, get("/reviews/999")).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(&f.app, get(&format!("/reviews/999/issues/{}", f.issue_id)))
            .await
            .0,
        StatusCode::NOT_FOUND,
        "an issue is only shown under its own review"
    );
}

#[tokio::test]
async fn text_from_claude_cannot_inject_html() {
    let f = fixture();
    let (_, _, body) = send(
        &f.app,
        get(&format!("/reviews/{}/issues/{}", f.review_id, f.issue_id)),
    )
    .await;
    assert!(
        !body.contains("<script>alert"),
        "raw script must never reach the page"
    );
    assert!(
        body.contains("&#60;script&#62;") || body.contains("&lt;script&gt;"),
        "{body}"
    );
}

// ── actions ──────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_human_can_edit_claudes_text_comment_resolve_and_rebase() {
    let f = fixture();
    let (status, headers, _) = send(
        &f.app,
        post(
            &format!("/comments/{}/edit", f.issue_comment_id),
            &format!("version=1&body={}", encode("Please use a digit here.")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        location(&headers),
        format!(
            "/reviews/{}/issues/{}#c{}",
            f.review_id, f.issue_id, f.issue_comment_id
        )
    );

    let (status, headers, _) = send(
        &f.app,
        post(
            &format!("/reviews/{}/issues/{}/comments", f.review_id, f.issue_id),
            "body=Agreed.",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(location(&headers).contains("#c"), "{headers:?}");

    let (status, _, _) = send(
        &f.app,
        post(
            &format!("/reviews/{}/issues/{}/status", f.review_id, f.issue_id),
            "status=resolved&comment=",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, _, _) = send(
        &f.app,
        post(&format!("/reviews/{}/base", f.review_id), "base="),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, _, _) = send(
        &f.app,
        post(
            &format!("/reviews/{}/title", f.review_id),
            "title=Use+digits",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let issue_id = f.issue_id;
    let review_id = f.review_id;
    let (issue, detail) = f
        .state
        .call(move |service| {
            Ok((
                service.get_issue(issue_id)?,
                service.get_review(review_id, None)?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(issue.issue.status, IssueStatus::Resolved);
    assert_eq!(issue.issue.resolved_by, Some(Actor::Human));
    assert_eq!(issue.comments[0].body, "Please use a digit here.");
    assert_eq!(
        issue.comments[0].author,
        Actor::Reviewer,
        "authorship never changes"
    );
    assert_eq!(issue.comments[0].edited_by, Some(Actor::Human));
    assert_eq!(issue.comments[1].author, Actor::Human);
    assert_eq!(detail.view.review.base_ref, None, "now a whole-tree review");
    assert_eq!(detail.view.review.title, "Use digits");

    let (_, _, body) = send(
        &f.app,
        get(&format!("/reviews/{}/issues/{}", f.review_id, f.issue_id)),
    )
    .await;
    assert!(body.contains("edited by you"), "the human's edit is marked");
    assert!(body.contains("Resolved by you"));
}

#[tokio::test]
async fn a_stale_edit_shows_both_versions_instead_of_overwriting() {
    let f = fixture();
    let url = format!("/comments/{}/edit", f.issue_comment_id);
    send(&f.app, post(&url, "version=1&body=first+edit")).await;
    let (status, _, body) = send(&f.app, post(&url, "version=1&body=my+stale+edit")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("first edit"), "the current version is shown");
    assert!(body.contains("my stale edit"), "the user's text is kept");
    assert!(
        body.contains("name=\"version\" value=\"2\""),
        "saving again targets the new version"
    );

    let summary_url = format!("/reviews/{}/summary", f.review_id);
    send(&f.app, post(&summary_url, "version=1&summary=new+summary")).await;
    let (status, _, body) = send(
        &f.app,
        post(&summary_url, "version=1&summary=stale+summary"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body.contains("stale summary") && body.contains("new summary"),
        "{body}"
    );
}

#[tokio::test]
async fn failed_actions_keep_the_submitted_text() {
    let f = fixture();
    let (status, _, body) = send(&f.app, post("/settings", "idle_timeout=2m")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("too short"), "{body}");
    assert!(body.contains("2m"), "{body}");

    let (status, _, body) = send(
        &f.app,
        post(&format!("/reviews/{}/comments", f.review_id), "body=+++"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("must not be empty"), "{body}");
}

#[tokio::test]
async fn settings_apply_to_the_running_server_and_persist() {
    let f = fixture();
    let (status, _, _) = send(&f.app, post("/settings", "idle_timeout=never")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(f.state.idle().timeout, IdleTimeout::Never);
    assert_eq!(f.state.idle().source, TimeoutSource::Saved);
    let saved = f
        .state
        .call(|service| service.idle_timeout())
        .await
        .unwrap();
    assert_eq!(saved, IdleTimeout::Never);
}

#[tokio::test]
async fn closing_and_reopening_a_review() {
    let f = fixture();
    let review_url = format!("/reviews/{}", f.review_id);
    send(&f.app, post(&format!("{review_url}/close"), "")).await;
    let (_, _, body) = send(&f.app, get(&review_url)).await;
    assert!(
        body.contains("Reopen review") && !body.contains("Edit summary"),
        "closed is read-only"
    );
    let (status, _, _) = send(&f.app, post(&format!("{review_url}/comments"), "body=late")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    send(&f.app, post(&format!("{review_url}/reopen"), "")).await;
    let (_, _, body) = send(&f.app, get(&review_url)).await;
    assert!(body.contains("Close review"));
}

#[tokio::test]
async fn browser_line_endings_do_not_count_as_edits() {
    let f = fixture();
    // Browsers submit textarea newlines as CRLF (%0D%0A); saving Claude's unchanged text must not
    // mark it "edited by you".
    let unchanged = "Use a digit.%0D%0A%0D%0A%3Cscript%3Ealert('pwned')%3C/script%3E";
    let (status, _, _) = send(
        &f.app,
        post(
            &format!("/comments/{}/edit", f.issue_comment_id),
            &format!("version=1&body={unchanged}"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    send(
        &f.app,
        post(
            &format!("/reviews/{}/comments", f.review_id),
            "body=first%0D%0Asecond",
        ),
    )
    .await;

    let (review_id, comment_id) = (f.review_id, f.issue_comment_id);
    let (comment, detail) = f
        .state
        .call(move |service| {
            Ok((
                service.get_comment(comment_id)?,
                service.get_review(review_id, None)?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(comment.version, 1);
    assert_eq!(comment.edited_by, None);
    let last = detail.summary_comments.last().unwrap();
    assert_eq!(last.body, "first\nsecond", "stored with LF line endings");
}

/// The values a browser shows in a page's textareas: the HTML parser drops one newline right
/// after a `<textarea>` start tag, then decodes character references.
fn textarea_values(page: &str) -> Vec<String> {
    page.split("<textarea")
        .skip(1)
        .map(|rest| {
            let content = &rest[rest.find('>').unwrap() + 1..rest.find("</textarea>").unwrap()];
            let content = content.strip_prefix('\n').unwrap_or(content);
            decode_references(content)
        })
        .collect()
}

/// Decode the character references an HTML escaper writes: `&#60;`, `&#x3C;` and `&lt;` forms.
fn decode_references(text: &str) -> String {
    let mut decoded = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        decoded.push_str(&rest[..start]);
        let end = start + rest[start..].find(';').unwrap();
        let name = &rest[start + 1..end];
        let character = match name {
            "lt" => '<',
            "gt" => '>',
            "amp" => '&',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let code = match name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                    Some(hex) => u32::from_str_radix(hex, 16).unwrap(),
                    None => name.strip_prefix('#').unwrap().parse().unwrap(),
                };
                char::from_u32(code).unwrap()
            }
        };
        decoded.push(character);
        rest = &rest[end + 1..];
    }
    decoded.push_str(rest);
    decoded
}

/// `text` as a form value, percent-encoded the way browsers submit it.
fn form_value(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' => {
                char::from(byte).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

#[tokio::test]
async fn text_that_starts_with_a_newline_survives_every_textarea() {
    let f = fixture();
    let review_url = format!("/reviews/{}", f.review_id);
    let issue_url = format!("{review_url}/issues/{}", f.issue_id);
    let summary = "\nAfter a blank line, <b>bold</b> & more.";
    let body = "\n\nTwo blank lines first.";
    send(
        &f.app,
        post(
            &format!("{review_url}/summary"),
            &format!("version=1&summary={}", form_value(summary)),
        ),
    )
    .await;
    let edit_url = format!("/comments/{}/edit", f.issue_comment_id);
    send(
        &f.app,
        post(&edit_url, &format!("version=1&body={}", form_value(body))),
    )
    .await;

    // Saving each editor's text back unchanged, as a browser submits it, is not an edit.
    let (_, _, page) = send(&f.app, get(&review_url)).await;
    assert!(
        textarea_values(&page).contains(&summary.to_string()),
        "{page}"
    );
    let (_, _, page) = send(&f.app, get(&issue_url)).await;
    assert!(textarea_values(&page).contains(&body.to_string()), "{page}");
    send(
        &f.app,
        post(
            &format!("{review_url}/summary"),
            &format!("version=2&summary={}", form_value(summary)),
        ),
    )
    .await;
    send(
        &f.app,
        post(&edit_url, &format!("version=2&body={}", form_value(body))),
    )
    .await;
    let (review_id, comment_id) = (f.review_id, f.issue_comment_id);
    let (comment, detail) = f
        .state
        .call(move |service| {
            Ok((
                service.get_comment(comment_id)?,
                service.get_review(review_id, None)?,
            ))
        })
        .await
        .unwrap();
    assert_eq!((comment.body.as_str(), comment.version), (body, 2));
    let review = &detail.view.review;
    assert_eq!(
        (review.summary.as_str(), review.summary_version),
        (summary, 2)
    );

    // A conflict page shows the text that was not saved.
    let mine = "\nMine.";
    let (status, _, page) = send(
        &f.app,
        post(&edit_url, &format!("version=1&body={}", form_value(mine))),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(textarea_values(&page), vec![mine.to_string()], "{page}");

    // So does an error page.
    send(&f.app, post(&format!("{review_url}/close"), "")).await;
    let late = "\nToo late.";
    let (status, _, page) = send(
        &f.app,
        post(
            &format!("{review_url}/comments"),
            &format!("body={}", form_value(late)),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(textarea_values(&page), vec![late.to_string()], "{page}");
}

#[tokio::test]
async fn the_issue_page_shows_its_location_in_the_webs_own_style() {
    let f = fixture();
    let issue_url = format!("/reviews/{}/issues/{}", f.review_id, f.issue_id);
    let shown_location = |page: &str| {
        let tag = "<code class=\"location\">";
        let start = page.find(tag).unwrap() + tag.len();
        page[start..start + page[start..].find("</code>").unwrap()].to_string()
    };
    let opened = f.repo.head();
    let opened_at = format!("src/lib.rs:5 @ {}", &opened[..7]);

    f.repo.write("other.txt", "x\n");
    let unrelated = f.repo.commit_all("touch another file");
    let (_, _, page) = send(&f.app, get(&issue_url)).await;
    assert_eq!(
        shown_location(&page),
        format!("{opened_at}; unchanged at {}", &unrelated[..7])
    );

    let file = numbered(20).replace("line 5\n", "line five\n");
    f.repo.write("src/lib.rs", &format!("new 1\nnew 2\n{file}"));
    let moved = f.repo.commit_all("insert above");
    let (_, _, page) = send(&f.app, get(&issue_url)).await;
    assert_eq!(
        shown_location(&page),
        format!("src/lib.rs:7 @ {} (opened at {opened_at})", &moved[..7])
    );

    f.repo.write(
        "src/lib.rs",
        &format!("new 1\nnew 2\n{}", file.replace("line five", "line 5")),
    );
    f.repo.commit_all("change the lines");
    let (_, _, page) = send(&f.app, get(&issue_url)).await;
    assert_eq!(
        shown_location(&page),
        opened_at,
        "the outdated notice explains the rest"
    );
    assert!(page.contains("Outdated: "), "{page}");
}

#[tokio::test]
async fn pages_still_render_when_the_repository_is_gone() {
    let f = fixture();
    let elsewhere = tempfile::tempdir().unwrap();
    let moved = elsewhere.path().join("moved");
    std::fs::rename(&f.repo.path, &moved).unwrap();

    let (status, _, body) = send(&f.app, get(&format!("/reviews/{}", f.review_id))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Repository not available"), "{body}");
    assert!(
        body.contains("Close review"),
        "the review can still be closed"
    );
    let (status, _, body) = send(
        &f.app,
        get(&format!("/reviews/{}/issues/{}", f.review_id, f.issue_id)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("Original code"),
        "the stored snippet is shown: {body}"
    );
    let (status, _, _) = send(&f.app, post(&format!("/reviews/{}/close", f.review_id), "")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    std::fs::rename(&moved, &f.repo.path).unwrap();
}

#[tokio::test]
async fn repositories_with_the_same_name_are_listed_separately() {
    let f = fixture();
    let first = TestRepo::named("api", &[("a.txt", "a\n")]);
    let second = TestRepo::named("api", &[("b.txt", "b\n")]);
    for repo in [&first, &second] {
        let path = repo.path.clone();
        f.state
            .call(move |service| {
                service.start_review(&StartReview {
                    repo_path: &path,
                    title: "Audit",
                    author: Actor::Reviewer,
                    base_mode: BaseMode::None,
                    summary: None,
                    head_ref: None,
                })
            })
            .await
            .unwrap();
    }
    let (_, _, body) = send(&f.app, get("/")).await;
    assert_eq!(body.matches("<h2>api <span").count(), 2, "{body}");
    for repo in [&first, &second] {
        let shown = lani::web::repo_location(&format!("{}/.git", repo.path.display()));
        assert!(body.contains(&shown), "{shown} missing from {body}");
    }
}

#[tokio::test]
async fn the_base_picker_shows_a_base_that_is_not_a_branch() {
    let f = fixture();
    f.repo.git(&["tag", "v1.0", "main"]);
    let review_id = f.review_id;
    f.state
        .call(move |service| {
            service.change_base(Actor::Human, review_id, &BaseMode::Ref("v1.0".into()))
        })
        .await
        .unwrap();

    let page = format!("/reviews/{review_id}");
    let (_, _, body) = send(&f.app, get(&page)).await;
    assert!(
        body.contains(
            r#"<option value="refs/tags/v1.0" selected>refs/tags/v1.0 (current)</option>"#
        ),
        "{body}"
    );
    assert!(
        !body.contains(r#"<option value="" selected>"#),
        "None must not look selected"
    );

    // Saving the shown base unchanged keeps it, even after the tag is deleted.
    f.repo.git(&["tag", "-d", "v1.0"]);
    let (_, _, body) = send(&f.app, get(&page)).await;
    assert!(
        body.contains("refs/tags/v1.0 (current; ref no longer exists)"),
        "{body}"
    );
    let (status, _, _) = send(
        &f.app,
        post(&format!("{page}/base"), "base=refs%2Ftags%2Fv1.0"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let detail = f
        .state
        .call(move |service| service.get_review(review_id, None))
        .await
        .unwrap();
    assert_eq!(
        detail.view.review.base_ref.as_deref(),
        Some("refs/tags/v1.0")
    );
}

// ── code views ───────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_files_view_lists_directories_and_marks_changes_and_issues() {
    let f = fixture();
    let files = format!("/reviews/{}/files", f.review_id);
    let (status, headers, _) = send(&f.app, get(&files)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), format!("{files}/"));

    let (status, _, page) = send(&f.app, get(&format!("{files}/"))).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(
        page.contains(&format!("href=\"{files}/src/\">src</a>")),
        "{page}"
    );
    assert!(page.contains("Changed"), "src holds a changed file: {page}");
    assert!(page.contains("1 open"), "src holds the open issue: {page}");
    assert!(
        page.contains("aria-current=\"page\">Files</a>"),
        "the Files tab is active"
    );

    // A directory URL without its slash, and a file URL with one, redirect to the real address.
    let (status, headers, _) = send(&f.app, get(&format!("{files}/src"))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), format!("{files}/src/"));
    let (status, headers, _) = send(&f.app, get(&format!("{files}/src/lib.rs/"))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), format!("{files}/src/lib.rs"));

    let (status, _, page) = send(&f.app, get(&format!("{files}/src/"))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        page.contains(&format!("href=\"{files}/src/lib.rs\">lib.rs</a>")),
        "{page}"
    );
    assert!(
        page.contains(&format!("href=\"{files}/\">..</a>")),
        "a link to the parent"
    );

    let (status, _, _) = send(&f.app, get(&format!("{files}/nowhere.rs"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = send(&f.app, get(&format!("{files}/src/%2E%2E/secret"))).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "`..` is refused, never resolved"
    );
}

#[tokio::test]
async fn the_file_view_highlights_code_and_shows_threads_under_their_lines() {
    let f = fixture();
    let url = format!("/reviews/{}/files/src/lib.rs", f.review_id);
    let (status, _, page) = send(&f.app, get(&url)).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(
        page.contains(&format!("data-commit=\"{}\"", f.repo.head())),
        "{page}"
    );
    assert!(page.contains("<tr id=\"L5\" data-line=\"5\">"), "{page}");
    assert!(
        page.contains("class=\"hl-"),
        "the code is highlighted: {page}"
    );
    assert!(page.contains("20 lines"), "{page}");
    // The issue on line 5 is shown right after line 5, before line 6.
    let thread = page.find(&format!("id=\"issue-{}\"", f.issue_id)).unwrap();
    let line5 = page.find("id=\"L5\"").unwrap();
    let line6 = page.find("id=\"L6\"").unwrap();
    assert!(line5 < thread && thread < line6, "{page}");
    assert!(page.contains("changed in this review"), "{page}");
    assert!(!page.contains("Other issues in this file"));

    // Once its lines change, the issue moves to the box above the code.
    f.repo.write(
        "src/lib.rs",
        &numbered(20).replace("line 5\n", "line FIVE\n"),
    );
    f.repo.commit_all("rewrite line 5");
    let (_, _, page) = send(&f.app, get(&url)).await;
    let other = page.find("Other issues in this file").unwrap();
    let thread = page.find(&format!("id=\"issue-{}\"", f.issue_id)).unwrap();
    assert!(
        other < thread && thread < page.find("id=\"L1\"").unwrap(),
        "{page}"
    );
    assert!(page.contains("Outdated: "), "{page}");
}

#[tokio::test]
async fn replies_from_a_code_view_return_to_it() {
    let f = fixture();
    let back = format!("/reviews/{}/files/src/lib.rs", f.review_id);
    let issue = format!("/reviews/{}/issues/{}", f.review_id, f.issue_id);
    let (status, headers, _) = send(
        &f.app,
        post(
            &format!("{issue}/comments"),
            &format!("body=Agreed.&back={}", back.replace('/', "%2F")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(
        location(&headers).starts_with(&format!("{back}#c")),
        "{headers:?}"
    );

    let (_, headers, _) = send(
        &f.app,
        post(
            &format!("{issue}/status"),
            &format!("status=resolved&back={}", back.replace('/', "%2F")),
        ),
    )
    .await;
    assert_eq!(location(&headers), format!("{back}#issue-{}", f.issue_id));

    // A return address outside LANI is ignored.
    let (_, headers, _) = send(
        &f.app,
        post(
            &format!("{issue}/comments"),
            "body=Again.&back=https%3A%2F%2Fexample.com%2F",
        ),
    )
    .await;
    assert!(
        location(&headers).starts_with(&format!("{issue}#c")),
        "{headers:?}"
    );
}

#[tokio::test]
async fn file_paths_are_encoded_in_links_and_decoded_in_requests() {
    let f = fixture();
    f.repo.write("docs/a b#c?.md", "# Title\n");
    f.repo.commit_all("odd name");
    let dir = format!("/reviews/{}/files/docs/", f.review_id);
    let (_, _, page) = send(&f.app, get(&dir)).await;
    let link = format!("/reviews/{}/files/docs/a%20b%23c%3F.md", f.review_id);
    assert!(page.contains(&format!("href=\"{link}\"")), "{page}");
    let (status, _, page) = send(&f.app, get(&link)).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("Markdown"), "{page}");
}

#[tokio::test]
async fn the_changes_view_shows_the_diff_with_threads_on_both_sides() {
    let f = fixture();
    let head = f.repo.head();
    let (review_id, repo_path) = (f.review_id, f.repo.path.clone());
    // A base-side issue on the removed line, and a head-side issue outside the hunk.
    f.state
        .call(move |service| {
            let issue = |side, line, text| CreateIssue {
                review_id,
                author: Actor::Reviewer,
                title: "A problem",
                severity: Severity::Medium,
                body: "Another.",
                spans: vec![SpanRequest {
                    path: "src/lib.rs",
                    side,
                    start_line: line,
                    end_line: line,
                    start_line_text: Some(text),
                }],
                commit: &head,
                worktree_path: if side == Side::Head {
                    Some(&repo_path)
                } else {
                    None
                },
                read_merge_base: None,
            };
            service.create_issue(&issue(Side::Base, 5, "line 5"))?;
            service.create_issue(&issue(Side::Head, 15, "line 15"))?;
            Ok(())
        })
        .await
        .unwrap();

    let url = format!("/reviews/{}/changes", f.review_id);
    let (status, _, page) = send(&f.app, get(&url)).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("aria-current=\"page\">Changes</a>"));
    assert!(page.contains("1 file · +1 −1"), "{page}");
    let anchor = "file-7372632f6c69622e7273"; // "src/lib.rs" in hex
    assert!(page.contains(&format!(
        "<section class=\"diff-file\" id=\"{anchor}\" data-live>"
    )));
    let row = |id: &str| page.find(&format!("id=\"{anchor}-{id}\"")).unwrap();
    let thread = |id: i64| page.find(&format!("id=\"issue-{id}\"")).unwrap();
    // The head-side issue follows the added line, the base-side issue the removed line.
    assert!(
        row("H5") < thread(f.issue_id) && thread(f.issue_id) < row("H6"),
        "{page}"
    );
    let base_issue = f.issue_id + 1;
    assert!(
        row("B5") < thread(base_issue) && thread(base_issue) < row("H5"),
        "{page}"
    );
    // Line 15 is outside the hunk, so its issue is listed above the diff.
    let outside = thread(f.issue_id + 2);
    assert!(page.find("not on the lines below").unwrap() < outside && outside < row("H2"));
    assert!(page.contains("data-merge-base=\""), "{page}");
}

#[tokio::test]
async fn large_binary_and_renamed_files_are_described_and_large_diffs_load_on_request() {
    let f = fixture();
    f.repo.write("big.txt", &numbered(1_600));
    f.repo.write("blob.bin", "a\0b\n");
    f.repo.git(&["mv", "src/lib.rs", "src/core.rs"]);
    f.repo.commit_all("big, binary and renamed");
    let url = format!("/reviews/{}/changes", f.review_id);
    let (_, _, page) = send(&f.app, get(&url)).await;
    assert!(page.contains("This diff changes 1600 lines"), "{page}");
    let expand = format!("{url}?expand=big.txt#file-");
    assert!(page.contains(&expand), "{page}");
    assert!(page.contains("Binary file, not shown."), "{page}");
    assert!(page.contains("src/lib.rs → src/core.rs"), "{page}");

    let (status, _, page) = send(&f.app, get(&format!("{url}?expand=big.txt"))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!page.contains("This diff changes 1600 lines"), "now loaded");
    assert!(page.contains("-H1600\""), "every added line is shown");
}

#[tokio::test]
async fn the_changes_view_does_not_depend_on_the_users_git_config() {
    let f = fixture();
    let repo = TestRepo::new(&[("notes.txt", "a\n\nb\nc\n")]);
    repo.switch_new("feature");
    repo.write("notes.txt", "a\n\nB\nc\n");
    repo.commit_all("capitalize b");
    // This prints the blank context line above the change as an empty line, not a single space.
    repo.git(&["config", "diff.suppressBlankEmpty", "true"]);
    let repo_path = repo.path.clone();
    let review_id = f
        .state
        .call(move |service| {
            let started = service.start_review(&StartReview {
                repo_path: &repo_path,
                title: "Capitalize b",
                author: Actor::Reviewer,
                base_mode: BaseMode::Auto,
                summary: None,
                head_ref: None,
            })?;
            Ok(started.view.review.id)
        })
        .await
        .unwrap();
    let (status, _, page) = send(&f.app, get(&format!("/reviews/{review_id}/changes"))).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("1 file · +1 −1"), "{page}");
    let anchor = "file-6e6f7465732e747874"; // "notes.txt" in hex
    assert!(
        page.contains(&format!("id=\"{anchor}-H2\"")),
        "the blank line is shown: {page}"
    );
}

#[tokio::test]
async fn span_links_lead_to_the_section_of_the_change_that_shows_them() {
    let f = fixture();
    let repo = TestRepo::new(&[("README.md", "Read me.\n"), ("src/lib.rs", &numbered(20))]);
    repo.switch_new("feature");
    repo.git(&["mv", "src/lib.rs", "src/core.rs"]);
    repo.write(
        "src/core.rs",
        &numbered(20).replace("line 5\n", "line five\n"),
    );
    let head = repo.commit_all("rename and spell out five");
    let repo_path = repo.path.clone();
    let (review_id, renamed, unchanged) = f
        .state
        .call(move |service| {
            let review_id = service
                .start_review(&StartReview {
                    repo_path: &repo_path,
                    title: "Rename",
                    author: Actor::Reviewer,
                    base_mode: BaseMode::Auto,
                    summary: None,
                    head_ref: None,
                })?
                .view
                .review
                .id;
            let issue = |path, side, text| CreateIssue {
                review_id,
                author: Actor::Reviewer,
                title: "A problem",
                severity: Severity::Low,
                body: "Look.",
                spans: vec![SpanRequest {
                    path,
                    side,
                    start_line: 1,
                    end_line: 1,
                    start_line_text: Some(text),
                }],
                commit: &head,
                worktree_path: if side == Side::Head {
                    Some(&repo_path)
                } else {
                    None
                },
                read_merge_base: None,
            };
            // The removed side of the renamed file is at its old path.
            let renamed = service.create_issue(&issue("src/lib.rs", Side::Base, "line 1"))?;
            let unchanged = service.create_issue(&issue("README.md", Side::Head, "Read me."))?;
            Ok((review_id, renamed.issue.id, unchanged.issue.id))
        })
        .await
        .unwrap();
    let issue_page = |issue_id: i64| format!("/reviews/{review_id}/issues/{issue_id}");

    let (status, _, page) = send(&f.app, get(&issue_page(renamed))).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let section = "file-7372632f636f72652e7273"; // "src/core.rs" in hex
    assert!(
        page.contains(&format!("href=\"/reviews/{review_id}/changes#{section}\"")),
        "the link leads to the renamed file's section: {page}"
    );
    let (_, _, page) = send(&f.app, get(&issue_page(unchanged))).await;
    assert!(page.contains(">View in file</a>"), "{page}");
    assert!(
        !page.contains("View in changes"),
        "the review does not change README.md: {page}"
    );
}

#[tokio::test]
async fn an_outdated_server_asks_to_be_restarted() {
    let f = fixture();
    let conn = rusqlite::Connection::open(f.home.db_path()).unwrap();
    conn.pragma_update(None, "user_version", 99).unwrap();
    let (status, _, page) = send(&f.app, get(&format!("/reviews/{}", f.review_id))).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{page}");
    assert!(page.contains("Restart needed"), "{page}");
    assert!(
        page.contains("schema version 99") && page.contains("run lani stop, then lani open"),
        "{page}"
    );
}

#[tokio::test]
async fn a_whole_tree_review_has_no_changes_view() {
    let f = fixture();
    let review_id = f.review_id;
    f.state
        .call(move |service| service.change_base(Actor::Human, review_id, &BaseMode::None))
        .await
        .unwrap();
    let (status, _, page) = send(&f.app, get(&format!("/reviews/{review_id}/changes"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(page.contains("whole-tree review"), "{page}");
    let (_, _, page) = send(&f.app, get(&format!("/reviews/{review_id}"))).await;
    assert!(!page.contains(">Changes</a>"), "no Changes tab: {page}");
}

#[tokio::test]
async fn issues_are_created_from_a_line_selection_at_the_rendered_commit() {
    let f = fixture();
    let head = f.repo.head();
    let create = format!("/reviews/{}/issues", f.review_id);
    let back = format!("/reviews/{}/files/src/lib.rs", f.review_id);
    let form = |side: &str, line: u32, merge_base: &str, back: &str| {
        format!(
            "title=Seven+is+odd&severity=low&path=src%2Flib.rs&side={side}&start_line={line}\
             &end_line={}&commit={head}&merge_base={merge_base}&body=Mine.&back={}",
            line + 1,
            back.replace('/', "%2F")
        )
    };

    let (status, headers, _) = send(&f.app, post(&create, &form("head", 7, "", &back))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let new_id = f.issue_id + 1;
    assert_eq!(location(&headers), format!("{back}#issue-{new_id}"));
    let view = f
        .state
        .call(move |service| service.get_issue(new_id))
        .await
        .unwrap();
    assert_eq!(view.issue.author, Actor::Human);
    assert_eq!(view.issue.title, "Seven is odd");
    assert_eq!(view.issue.severity, Severity::Low);
    assert_eq!(
        view.issue.anchors[0].commit, head,
        "anchored to the rendered commit"
    );
    assert_eq!(
        (
            view.issue.anchors[0].start_line,
            view.issue.anchors[0].end_line
        ),
        (7, 8)
    );

    // A base-side selection must come from the current merge-base.
    let merge_base = f.repo.git(&["merge-base", "main", "feature"]);
    let stale = "0".repeat(39) + "1";
    let (status, _, page) = send(&f.app, post(&create, &form("base", 5, &stale, &back))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(page.contains("the base moved"), "{page}");
    assert!(page.contains("Mine."), "the text is kept: {page}");
    let (status, headers, _) =
        send(&f.app, post(&create, &form("base", 5, &merge_base, &back))).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{headers:?}");

    let (status, _, _) = send(&f.app, post(&create, &form("left", 5, "", &back))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // A severity must be chosen, and a title is one line.
    let no_severity = form("head", 5, "", &back).replace("severity=low", "severity=");
    let (status, _, page) = send(&f.app, post(&create, &no_severity)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        page.contains("Choose a severity") && page.contains("Mine."),
        "{page}"
    );
    let two_lines = form("head", 5, "", &back).replace("Seven+is+odd", "One%0ATwo");
    let (status, _, page) = send(&f.app, post(&create, &two_lines)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(page.contains("single line"), "{page}");
    // A return address outside LANI is ignored.
    let (_, headers, _) = send(
        &f.app,
        post(&create, &form("head", 9, "", "https://example.com/")),
    )
    .await;
    assert!(
        location(&headers).starts_with(&format!("/reviews/{}#issue-", f.review_id)),
        "{headers:?}"
    );
}

#[tokio::test]
async fn titles_and_severities_are_shown_and_edited_with_version_checks() {
    let f = fixture();
    let review = format!("/reviews/{}", f.review_id);
    let issue = format!("{review}/issues/{}", f.issue_id);
    let (_, _, page) = send(&f.app, get(&review)).await;
    assert!(
        page.contains(">A problem</a>"),
        "the list shows the title: {page}"
    );
    assert!(
        page.contains("class=\"badge sev-medium\">medium<"),
        "{page}"
    );

    let details = format!("{issue}/details");
    let (status, headers, _) = send(
        &f.app,
        post(&details, "title=Use+a+digit&severity=nit&version=1"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), issue);
    let (_, _, page) = send(&f.app, get(&issue)).await;
    assert!(
        page.contains("Use a digit <span class=\"muted\">#"),
        "{page}"
    );
    assert!(page.contains("class=\"badge sev-nit\">nit<"), "{page}");
    assert!(page.contains("title or severity edited by you"), "{page}");
    assert!(
        page.contains("<option value=\"nit\" selected>"),
        "the picker shows it: {page}"
    );

    // A stale version is refused and says what the issue is now.
    let (status, _, page) =
        send(&f.app, post(&details, "title=Mine&severity=high&version=1")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(page.contains("now version 2"), "{page}");
    // Saving unchanged values is not an edit.
    send(
        &f.app,
        post(&details, "title=Use+a+digit&severity=nit&version=2"),
    )
    .await;
    let issue_id = f.issue_id;
    let view = f
        .state
        .call(move |service| service.get_issue(issue_id))
        .await
        .unwrap();
    assert_eq!(
        (view.issue.version, view.issue.edited_by),
        (2, Some(Actor::Human))
    );
    let (_, _, page) = send(&f.app, get(&review)).await;
    assert!(
        page.contains("changed the title or severity of issue #"),
        "{page}"
    );
}

#[tokio::test]
async fn the_code_views_carry_the_script_and_the_forms_only_while_editable() {
    let f = fixture();
    let (status, headers, script) = send(&f.app, get("/static/lani.js")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/javascript")
    );
    assert!(script.contains("new-issue-template"));

    let (status, _, html) = send(&f.app, post("/preview", "body=**hi**+%3Cb%3E")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("<strong>hi</strong>") && !html.contains("<b>"),
        "{html}"
    );

    let file = format!("/reviews/{}/files/src/lib.rs", f.review_id);
    let changes = format!("/reviews/{}/changes", f.review_id);
    for url in [&file, &changes] {
        let (_, _, page) = send(&f.app, get(url)).await;
        assert!(
            page.contains("<script src=\"/static/lani.js\" defer></script>"),
            "{url}"
        );
        assert!(page.contains("id=\"new-issue-template\""), "{url}");
        assert!(
            page.contains(&format!("action=\"/reviews/{}/issues\"", f.review_id)),
            "{url}"
        );
    }
    send(&f.app, post(&format!("/reviews/{}/close", f.review_id), "")).await;
    for url in [&file, &changes] {
        let (_, _, page) = send(&f.app, get(url)).await;
        assert!(!page.contains("new-issue-template"), "closed: {url}");
    }
}

#[tokio::test]
async fn an_issue_with_several_spans_shows_one_thread_and_markers_elsewhere() {
    let f = fixture();
    f.repo.write("docs/notes.md", "# Notes\nfive\n");
    let head = f.repo.commit_all("notes");
    let (review_id, repo_path) = (f.review_id, f.repo.path.clone());
    let issue_id = f
        .state
        .call(move |service| {
            let span = |path, line, text| SpanRequest {
                path,
                side: Side::Head,
                start_line: line,
                end_line: line,
                start_line_text: Some(text),
            };
            let view = service.create_issue(&CreateIssue {
                review_id,
                author: Actor::Reviewer,
                title: "Spelled-out numbers",
                severity: Severity::Low,
                body: "Pick one style.",
                spans: vec![
                    span("src/lib.rs", 5, "line five"),
                    span("src/lib.rs", 12, "line 12"),
                    span("docs/notes.md", 2, "five"),
                ],
                commit: &head,
                worktree_path: Some(&repo_path),
                read_merge_base: None,
            })?;
            Ok(view.issue.id)
        })
        .await
        .unwrap();
    let thread = format!("id=\"issue-{issue_id}\"");
    let marker = format!("class=\"span-marker\" href=\"#issue-{issue_id}\"");

    // Files view: the thread under the first span in the file, a marker on the later one.
    let file = format!("/reviews/{}/files/src/lib.rs", f.review_id);
    let (_, _, page) = send(&f.app, get(&file)).await;
    let at = |needle: &str| {
        page.find(needle)
            .unwrap_or_else(|| panic!("{needle}: {page}"))
    };
    assert!(
        at("id=\"L5\"") < at(&thread) && at(&thread) < at("id=\"L6\""),
        "{page}"
    );
    assert!(
        at("id=\"L12\"") < at(&marker) && at(&marker) < at("id=\"L13\""),
        "{page}"
    );
    assert_eq!(page.matches(&thread).count(), 1, "one thread per file");
    assert!(
        page.contains("title=\"Issue #"),
        "the marker names the issue: {page}"
    );

    // Changes view: one thread on the page, at the first span in page order (docs/ sorts
    // before src/), and markers on the spans in the other file.
    let changes = format!("/reviews/{}/changes", f.review_id);
    let (_, _, page) = send(&f.app, get(&changes)).await;
    assert_eq!(page.matches(&thread).count(), 1, "one thread per page");
    let docs = page.find("id=\"file-646f63732f6e6f7465732e6d64\"").unwrap();
    let lib = page.find("id=\"file-7372632f6c69622e7273\"").unwrap();
    let thread_at = page.find(&thread).unwrap();
    assert!(docs < thread_at && thread_at < lib, "{page}");
    assert!(
        page[lib..].contains(&marker),
        "a marker in src/lib.rs: {page}"
    );

    // The tree counts the issue once for docs/ and once for src/, and once per file.
    let (_, _, page) = send(&f.app, get(&format!("/reviews/{}/files/", f.review_id))).await;
    assert!(
        page.contains("1 open</span>"),
        "docs/ holds one open issue: {page}"
    );
    assert!(
        page.contains("2 open</span>"),
        "src/ holds two, not three: {page}"
    );

    // The issue page shows every span with its code.
    let (_, _, page) = send(
        &f.app,
        get(&format!("/reviews/{}/issues/{issue_id}", f.review_id)),
    )
    .await;
    assert_eq!(page.matches("class=\"span\"").count(), 3, "{page}");
    assert!(page.contains("Span 3 · Code @ "), "{page}");
}

#[tokio::test]
async fn a_thread_goes_under_its_first_shown_span_even_in_a_later_file() {
    let f = fixture();
    f.repo.write("tests/lib.rs", "five\n");
    let head = f.repo.commit_all("a test");
    let (review_id, repo_path) = (f.review_id, f.repo.path.clone());
    // src/lib.rs line 15 is outside the file's hunk; tests/lib.rs line 1 is an added line.
    let issue_id = f
        .state
        .call(move |service| {
            let span = |path, line, text| SpanRequest {
                path,
                side: Side::Head,
                start_line: line,
                end_line: line,
                start_line_text: Some(text),
            };
            let view = service.create_issue(&CreateIssue {
                review_id,
                author: Actor::Reviewer,
                title: "Spelled-out numbers",
                severity: Severity::Low,
                body: "Pick one style.",
                spans: vec![
                    span("src/lib.rs", 15, "line 15"),
                    span("tests/lib.rs", 1, "five"),
                ],
                commit: &head,
                worktree_path: Some(&repo_path),
                read_merge_base: None,
            })?;
            Ok(view.issue.id)
        })
        .await
        .unwrap();
    let (_, _, page) = send(&f.app, get(&format!("/reviews/{review_id}/changes"))).await;
    let thread = format!("id=\"issue-{issue_id}\"");
    assert_eq!(page.matches(&thread).count(), 1, "one thread per page");
    let tests = "file-74657374732f6c69622e7273"; // "tests/lib.rs" in hex
    let at = |needle: &str| {
        page.find(needle)
            .unwrap_or_else(|| panic!("{needle}: {page}"))
    };
    assert!(
        at(&format!("id=\"{tests}-H1\"")) < at(&thread),
        "under its span in tests/lib.rs, not in the box of src/lib.rs: {page}"
    );
}

#[tokio::test]
async fn turns_and_questions_for_the_human_are_shown_and_the_human_hands_on() {
    let f = fixture();
    let (review_id, issue_id) = (f.review_id, f.issue_id);
    let head = f.repo.head();
    let repo_path = f.repo.path.clone();
    let second = f
        .state
        .call(move |service| {
            // A second issue, then a question about the first: questions sort first.
            let second = service.create_issue(&CreateIssue {
                review_id,
                author: Actor::Reviewer,
                title: "Another",
                severity: Severity::Nit,
                body: "b",
                spans: vec![SpanRequest {
                    path: "src/lib.rs",
                    side: Side::Head,
                    start_line: 9,
                    end_line: 9,
                    start_line_text: Some("line 9"),
                }],
                commit: &head,
                worktree_path: Some(&repo_path),
                read_merge_base: None,
            })?;
            service.ask_human(Actor::Reviewer, second.issue.id, "Keep it?")?;
            service.hand_off(
                Actor::Reviewer,
                review_id,
                Some(Actor::Implementer),
                Some("Please **fix** issue 1."),
            )?;
            Ok(second.issue.id)
        })
        .await
        .unwrap();

    let review_url = format!("/reviews/{review_id}");
    let (_, _, page) = send(&f.app, get(&review_url)).await;
    assert!(page.contains("<strong>Waiting for:</strong> <span class=\"author author-implementer\">Claude · implementer</span>"), "{page}");
    assert!(
        page.contains("Please <strong>fix</strong> issue 1."),
        "the note is rendered: {page}"
    );
    let question =
        format!("<a href=\"/reviews/{review_id}/issues/{second}\">#{second} Another</a>");
    assert!(
        page.contains(&question),
        "questions link to their issues: {page}"
    );
    let list = page.find("class=\"issue-list\"").unwrap();
    let first_row = page[list..].find(&format!("#{second}</a>")).unwrap();
    let other_row = page[list..].find(&format!("#{issue_id}</a>")).unwrap();
    assert!(
        first_row < other_row,
        "the issue that needs you comes first: {page}"
    );
    assert!(page[list..].contains("Needs you"), "{page}");

    let (_, _, index) = send(&f.app, get("/")).await;
    assert!(index.contains("Needs you"), "{index}");
    assert!(
        index.contains("waiting for <span class=\"author author-implementer\">"),
        "{index}"
    );
    let (_, _, issue_page) = send(&f.app, get(&format!("{review_url}/issues/{second}"))).await;
    assert!(
        issue_page.contains("Claude · reviewer asked you about this issue"),
        "{issue_page}"
    );

    // Replying answers the question.
    send(
        &f.app,
        post(
            &format!("{review_url}/issues/{second}/comments"),
            "body=Keep+it.",
        ),
    )
    .await;
    let (_, _, page) = send(&f.app, get(&review_url)).await;
    assert!(!page.contains("Questions for you"), "{page}");

    // The human hands the review on.
    let turn = format!("{review_url}/turn");
    let (status, headers, _) =
        send(&f.app, post(&turn, "to=reviewer&note=Have+another+look.")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), review_url);
    let (_, _, page) = send(&f.app, get(&review_url)).await;
    assert!(page.contains("author-reviewer\">Claude · reviewer</span> <span class=\"muted small\">· handed over by you"), "{page}");
    let (status, _, _) = send(&f.app, post(&turn, "to=human")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    send(&f.app, post(&turn, "to=nobody")).await;
    let (_, _, page) = send(&f.app, get(&review_url)).await;
    assert!(!page.contains("Waiting for:"), "{page}");

    // The round limit is a setting.
    let (_, _, settings) = send(&f.app, get("/settings")).await;
    assert!(
        settings.contains("name=\"round_limit\" value=\"5\""),
        "{settings}"
    );
    let (status, _, _) = send(&f.app, post("/settings/round-limit", "round_limit=3")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_, _, settings) = send(&f.app, get("/settings")).await;
    assert!(settings.contains("value=\"3\""), "{settings}");
    let (status, _, _) = send(&f.app, post("/settings/round-limit", "round_limit=0")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ── live updates (M4) ────────────────────────────────────────────────────────────────────────

/// A live-update stream, read one event at a time.
struct EventStream(BodyDataStream);

impl EventStream {
    async fn open(app: &Router, request: Request<Body>) -> Self {
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/event-stream"
        );
        Self(response.into_body().into_data_stream())
    }

    /// The next event, or `None` once the stream has ended; `Err` if none arrives `within`.
    async fn next_within(
        &mut self,
        within: Duration,
    ) -> Result<Option<String>, tokio::time::error::Elapsed> {
        tokio::time::timeout(within, async {
            loop {
                let chunk = self.0.next().await?.unwrap();
                let text = String::from_utf8(chunk.to_vec()).unwrap();
                // Keep-alive comments start with a colon.
                if !text.starts_with(':') {
                    return Some(text);
                }
            }
        })
        .await
    }

    async fn next(&mut self) -> Option<String> {
        self.next_within(Duration::from_secs(5))
            .await
            .expect("an event within 5 s")
    }
}

/// The latest event a review page was rendered with.
fn events_after(page: &str) -> i64 {
    let start = page.find("data-after=\"").unwrap() + "data-after=\"".len();
    page[start..].split('"').next().unwrap().parse().unwrap()
}

/// An event's `id:` field.
fn event_id(event: &str) -> i64 {
    event
        .lines()
        .find_map(|line| line.strip_prefix("id: "))
        .unwrap_or_else(|| panic!("no id: {event}"))
        .parse()
        .unwrap()
}

#[tokio::test]
async fn review_pages_follow_the_live_update_stream_until_the_server_stops() {
    let f = fixture();
    let review_id = f.review_id;
    let review = format!("/reviews/{review_id}");
    for path in [
        review.clone(),
        format!("{review}/issues/{}", f.issue_id),
        format!("{review}/files/"),
        format!("{review}/files/src/lib.rs"),
        format!("{review}/changes"),
    ] {
        let (status, _, page) = send(&f.app, get(&path)).await;
        assert_eq!(status, StatusCode::OK, "{path}: {page}");
        assert!(
            page.contains(&format!("data-events=\"{review}/events\""))
                && page.contains("<script src=\"/static/lani.js\" defer></script>")
                && page.contains("id=\"review-head\" data-live"),
            "{path}: {page}"
        );
    }
    let (status, _, _) = send(&f.app, get("/reviews/999/events")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (_, _, page) = send(&f.app, get(&review)).await;
    let after = events_after(&page);
    // A reconnecting browser's Last-Event-ID wins over the `after` the page was rendered with;
    // nothing has changed since either.
    let mut request = get(&format!("{review}/events?after=0"));
    request
        .headers_mut()
        .insert("last-event-id", after.to_string().parse().unwrap());
    let mut stream = EventStream::open(&f.app, request).await;
    assert!(
        stream
            .next_within(Duration::from_millis(1200))
            .await
            .is_err(),
        "no change, no event"
    );

    f.state
        .call(move |service| {
            service.add_comment(
                Actor::Reviewer,
                CommentTarget::Summary(review_id),
                "Another pass.",
            )
        })
        .await
        .unwrap();
    let event = stream.next().await.unwrap();
    assert!(
        event.starts_with("event: changed\n") && event.contains("data: events\n"),
        "{event}"
    );
    let changed = event_id(&event);
    assert!(changed > after, "{event}");
    // A page rendered before that change hears about it as soon as it connects.
    let mut late = EventStream::open(&f.app, get(&format!("{review}/events?after={after}"))).await;
    let event = late.next().await.unwrap();
    assert_eq!(event_id(&event), changed, "{event}");

    f.state.request_shutdown(Stop::Requested);
    for stream in [&mut stream, &mut late] {
        let event = stream.next().await.unwrap();
        assert!(
            event.starts_with("event: stopping\n")
                && event.contains(
                    "data: LANI was stopped. Run lani open, or ask Claude to open it again."
                ),
            "{event}"
        );
        assert_eq!(stream.next().await, None, "the stream ends");
    }
}

#[tokio::test]
async fn a_server_older_than_the_installed_lani_says_so() {
    let bin = tempfile::tempdir().unwrap();
    let installed = bin.path().join("lani");
    std::fs::write(&installed, "old").unwrap();
    let f = fixture_with(Some(Executable::at(&installed).unwrap()));
    let banner = "A newer lani has been installed since this web server started";
    let review = format!("/reviews/{}", f.review_id);
    let (_, _, page) = send(&f.app, get(&review)).await;
    assert!(!page.contains(banner), "{page}");
    let mut stream = EventStream::open(
        &f.app,
        get(&format!("{review}/events?after={}", events_after(&page))),
    )
    .await;

    // Replaced the way `cargo install` replaces the installed binary.
    std::fs::write(bin.path().join("lani.new"), "new").unwrap();
    std::fs::rename(bin.path().join("lani.new"), &installed).unwrap();
    let event = stream.next().await.unwrap();
    assert!(
        event.starts_with("event: changed\n") && event.contains("data: update\n"),
        "{event}"
    );
    for path in [
        "/".to_string(),
        "/settings".to_string(),
        review.clone(),
        format!("{review}/issues/{}", f.issue_id),
        format!("{review}/files/"),
        format!("{review}/files/src/lib.rs"),
        format!("{review}/changes"),
    ] {
        let (_, _, page) = send(&f.app, get(&path)).await;
        assert!(
            page.contains(banner) && page.contains("run <code>lani stop</code>"),
            "{path}: {page}"
        );
    }
}

#[tokio::test]
async fn a_live_update_stream_ends_once_a_newer_lani_upgrades_the_database() {
    let f = fixture();
    let mut stream =
        EventStream::open(&f.app, get(&format!("/reviews/{}/events", f.review_id))).await;
    let conn = rusqlite::Connection::open(f.home.db_path()).unwrap();
    conn.pragma_update(None, "user_version", 99).unwrap();
    let event = stream.next().await.unwrap();
    assert!(
        event.starts_with("event: failed\n")
            && event.contains("data: This web server is out of date: a newer lani"),
        "{event}"
    );
    assert_eq!(stream.next().await, None, "the stream ends");
}
