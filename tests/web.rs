//! Web UI tests: requests go through the real router and middleware in-process.

mod common;

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{TestHome, TestRepo, numbered};
use lani::base::BaseMode;
use lani::model::{Actor, IssueStatus, Side};
use lani::service::{CommentTarget, CreateIssue, Service, StartReview};
use lani::settings::IdleTimeout;
use lani::web::{self, AppState, Instance, TimeoutSource};
use tower::ServiceExt as _;

const PORT: u16 = 4242;
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const HOST: &str = "127.0.0.1:4242";
const ORIGIN: &str = "http://127.0.0.1:4242";

struct Fixture {
    _home: TestHome,
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
            body: "Use a digit.\n\n<script>alert('pwned')</script>",
            path: "src/lib.rs",
            side: Side::Head,
            start_line: 5,
            end_line: 5,
            start_line_text: Some("line five"),
            commit: &head,
            worktree_path: Some(&repo.path),
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
        IdleTimeout::After(Duration::from_secs(3 * 3600)),
        TimeoutSource::Saved,
    );
    Fixture {
        app: web::router(state.clone()),
        state,
        review_id,
        issue_id: issue.issue.id,
        issue_comment_id: issue.comments[0].id,
        _home: home,
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
