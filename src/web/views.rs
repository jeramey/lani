//! View models for templates, and the page-level error type.
//!
//! Display strings are computed here rather than in templates, so templates stay free of logic
//! and every value they print is auto-escaped (only Markdown output is marked safe).

use std::fmt::Write as _;

use askama::Template;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

use crate::error::Error;
use crate::model::{
    Actor, BaseSource, Comment, Event, EventKind, IssueStatus, ReviewStatus, Side, short_ref,
    short_sha,
};
use crate::service::{IssueView, Location, ReviewView};
use crate::{highlight, markdown, time};

// ── URLs ─────────────────────────────────────────────────────────────────────────────────────

#[must_use]
pub fn review_url(review_id: i64) -> String {
    format!("/reviews/{review_id}")
}

#[must_use]
pub fn issue_url(review_id: i64, issue_id: i64) -> String {
    format!("/reviews/{review_id}/issues/{issue_id}")
}

/// The Files view of `path` (`""` for the root). Directory URLs end with `/`; file URLs don't.
#[must_use]
pub fn files_url(review_id: i64, path: &str, directory: bool) -> String {
    let mut url = format!("/reviews/{review_id}/files/");
    if !path.is_empty() {
        url.push_str(&encode_path(path));
        if directory {
            url.push('/');
        }
    }
    url
}

/// The Changes (diff) view of a review.
#[must_use]
pub fn changes_url(review_id: i64) -> String {
    format!("/reviews/{review_id}/changes")
}

/// A view of a review that `lani open` and the `open_web_ui` tool can open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Overview,
    Changes,
    Files,
}

/// The page path for a review's `view`, at `file` and `line` when given: the Files view at
/// `#L<line>`, or the Changes view at the file's diff (and the line on its head side).
pub fn review_page_path(
    review_id: i64,
    view: View,
    file: Option<&str>,
    line: Option<u32>,
) -> crate::error::Result<String> {
    if let Some(file) = file {
        crate::anchor::validate_path(file)?;
    }
    let invalid = |message: &str| Err(Error::Invalid(message.to_string()));
    match (view, file, line) {
        (_, None, Some(_)) => invalid("a line needs a file: give the file's path too"),
        (View::Overview, Some(_), _) => {
            invalid("the overview shows no file; choose the files or changes view")
        }
        (View::Overview, None, None) => Ok(review_url(review_id)),
        (View::Files, None, None) => Ok(files_url(review_id, "", true)),
        (View::Files, Some(file), line) => {
            // A directory's URL ends with "/"; the server redirects there if `file` is one.
            let url = files_url(review_id, file, false);
            Ok(match line {
                Some(line) => format!("{url}#L{line}"),
                None => url,
            })
        }
        (View::Changes, None, None) => Ok(changes_url(review_id)),
        (View::Changes, Some(file), line) => {
            let anchor = file_anchor(file);
            Ok(match line {
                Some(line) => format!("{}#{anchor}-H{line}", changes_url(review_id)),
                None => format!("{}#{anchor}", changes_url(review_id)),
            })
        }
    }
}

/// A repository path as a URL path: every byte outside `A-Z a-z 0-9 - . _ ~` is
/// percent-encoded, except the `/` between segments.
#[must_use]
pub fn encode_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

/// One step of a path's breadcrumb trail; the last step has no link.
pub struct Crumb {
    pub label: String,
    pub url: Option<String>,
}

/// The breadcrumb trail of `path` in the Files view, starting at the repository root.
#[must_use]
pub fn crumbs(review_id: i64, repo_name: &str, path: &str) -> Vec<Crumb> {
    let mut crumbs = vec![Crumb {
        label: repo_name.to_string(),
        url: Some(files_url(review_id, "", true)),
    }];
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    for (index, segment) in segments.iter().enumerate() {
        let is_last = index + 1 == segments.len();
        crumbs.push(Crumb {
            label: (*segment).to_string(),
            url: (!is_last).then(|| files_url(review_id, &segments[..=index].join("/"), true)),
        });
    }
    if let Some(last) = crumbs.last_mut()
        && path.is_empty()
    {
        last.url = None;
    }
    crumbs
}

/// Where a comment is shown: on its issue's page, or on the review page for summary comments.
#[must_use]
pub fn comment_url(comment: &Comment) -> String {
    let page = match comment.issue_id {
        Some(issue_id) => issue_url(comment.review_id, issue_id),
        None => review_url(comment.review_id),
    };
    format!("{page}#c{}", comment.id)
}

/// Where a repository lives, for display: its main worktree (the parent of a `.git` directory)
/// or the git directory of a bare repository, with the home directory shown as `~`.
#[must_use]
pub fn repo_location(git_dir: &str) -> String {
    let path = std::path::Path::new(git_dir);
    let location = if path.file_name().is_some_and(|name| name == ".git") {
        path.parent().unwrap_or(path)
    } else {
        path
    };
    let location = location.to_string_lossy();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && location.starts_with(&format!("{home}/")) => {
            format!("~{}", &location[home.len()..])
        }
        _ => location.into_owned(),
    }
}

// ── shared pieces ────────────────────────────────────────────────────────────────────────────

/// How an author is shown: "You" for the human, "Claude · <role>" otherwise.
#[derive(Clone, Copy, Debug)]
pub struct AuthorView {
    pub label: &'static str,
    /// CSS class giving each kind of author a distinct accent.
    pub class: &'static str,
}

#[must_use]
pub fn author(actor: Actor) -> AuthorView {
    match actor {
        Actor::Human => AuthorView {
            label: "You",
            class: "author-human",
        },
        Actor::Reviewer => AuthorView {
            label: "Claude · reviewer",
            class: "author-reviewer",
        },
        Actor::Implementer => AuthorView {
            label: "Claude · implementer",
            class: "author-implementer",
        },
    }
}

pub struct CommentView {
    pub anchor: String,
    pub author: AuthorView,
    pub created: String,
    pub at_sha: String,
    pub edited: Option<String>,
    pub body_html: String,
    pub body: String,
    pub version: i64,
    pub edit_action: String,
    /// The page to return to after an edit, when it is not the issue page.
    pub back: Option<String>,
}

#[must_use]
pub fn comment(comment: &Comment) -> CommentView {
    let edited = comment.edited_by.map(|editor| {
        if editor == comment.author {
            "edited".to_string()
        } else if editor == Actor::Human {
            "edited by you".to_string()
        } else {
            format!("edited by {}", author(editor).label)
        }
    });
    CommentView {
        anchor: format!("c{}", comment.id),
        author: author(comment.author),
        created: time::to_local(&comment.created_at),
        at_sha: short_sha(&comment.head_sha).to_string(),
        edited,
        body_html: markdown::render(&comment.body),
        body: comment.body.clone(),
        version: comment.version,
        edit_action: format!("/comments/{}/edit", comment.id),
        back: None,
    }
}

/// Everything shown about a review at the top of its pages.
pub struct ReviewHeader {
    pub id: i64,
    pub url: String,
    pub title: String,
    pub repo: String,
    pub status_label: &'static str,
    pub status_class: &'static str,
    pub head: String,
    pub base: String,
    /// E.g. "auto-detected"; `None` for an explicitly chosen base.
    pub base_note: Option<&'static str>,
    pub whole_tree: bool,
    pub created_by: AuthorView,
    pub created: String,
    /// Why current code cannot be shown, e.g. the repository was moved or deleted.
    pub repo_notice: Option<String>,
    pub files_url: String,
    /// `None` for a whole-tree review, which has no diff.
    pub changes_url: Option<String>,
    /// The active tab: "overview", "changes" or "files"; `""` for none.
    pub tab: &'static str,
}

#[must_use]
pub fn review_header(view: &ReviewView) -> ReviewHeader {
    let review = &view.review;
    let git = view.git.as_ref();
    let head = match git.map(|git| git.head_sha.as_deref()) {
        Some(Some(sha)) => format!("{} @ {}", short_ref(&review.head_ref), short_sha(sha)),
        Some(None) => format!("{} (branch no longer exists)", short_ref(&review.head_ref)),
        None => short_ref(&review.head_ref).to_string(),
    };
    let base = match &review.base_ref {
        None => "None — whole-tree review".to_string(),
        Some(base_ref) => {
            let mut text = short_ref(base_ref).to_string();
            if let Some(git) = git {
                if git.base_sha.is_none() {
                    text.push_str(" (ref no longer exists)");
                }
                if let Some(merge_base) = &git.merge_base {
                    let _ = write!(text, " · merge-base {}", short_sha(merge_base));
                }
                if let Some(ahead) = git.commits_ahead {
                    let plural = if ahead == 1 { "" } else { "s" };
                    let _ = write!(text, " · {ahead} commit{plural} under review");
                }
            }
            text
        }
    };
    let base_note = match review.base_source {
        BaseSource::Auto => Some("auto-detected"),
        BaseSource::Explicit => None,
        BaseSource::WebUi => Some("set in the web UI"),
    };
    let open = review.status == ReviewStatus::Open;
    ReviewHeader {
        id: review.id,
        url: review_url(review.id),
        title: review.title.clone(),
        repo: view.repo.name.clone(),
        status_label: if open { "Open" } else { "Closed" },
        status_class: if open { "open" } else { "closed" },
        head,
        base,
        base_note,
        whole_tree: review.base_ref.is_none(),
        created_by: author(review.created_by),
        created: time::to_local(&review.created_at),
        repo_notice: view.repo_unavailable.clone(),
        files_url: files_url(review.id, "", true),
        changes_url: review.base_ref.is_some().then(|| changes_url(review.id)),
        tab: "overview",
    }
}

/// One row in a review's issue list.
pub struct IssueRow {
    pub id: i64,
    pub url: String,
    pub status_label: String,
    pub status_class: &'static str,
    pub location: String,
    pub outdated: bool,
    /// The first line of the issue's description as inline HTML (sanitized by the renderer).
    pub preview_html: String,
    pub comments: String,
    pub last_author: Option<AuthorView>,
}

#[must_use]
pub fn issue_row(view: &IssueView) -> IssueRow {
    let issue = &view.issue;
    let (location, outdated) = match &view.location {
        Location::Current {
            path,
            start_line,
            end_line,
            ..
        } => (format!("{path}:{}", lines(*start_line, *end_line)), false),
        Location::Outdated { .. } => (
            format!(
                "{}:{}",
                issue.anchor.path,
                lines(issue.anchor.start_line, issue.anchor.end_line)
            ),
            true,
        ),
    };
    let location = if issue.anchor.side == Side::Base {
        format!("{location} (base side)")
    } else {
        location
    };
    let (status_label, status_class) = issue_status(issue.status, issue.resolved_by);
    IssueRow {
        id: issue.id,
        url: issue_url(issue.review_id, issue.id),
        status_label,
        status_class,
        location,
        outdated,
        preview_html: view
            .comments
            .first()
            .map(|comment| markdown::render_inline(&first_line(&comment.body)))
            .unwrap_or_default(),
        comments: match view.comments.len() {
            1 => "1 comment".to_string(),
            count => format!("{count} comments"),
        },
        last_author: view.comments.last().map(|comment| author(comment.author)),
    }
}

#[must_use]
pub fn issue_status(status: IssueStatus, resolved_by: Option<Actor>) -> (String, &'static str) {
    match (status, resolved_by) {
        (IssueStatus::Resolved, Some(actor)) => {
            let who = if actor == Actor::Human {
                "you"
            } else {
                author(actor).label
            };
            (format!("Resolved by {who}"), "resolved")
        }
        (IssueStatus::Resolved, None) => ("Resolved".to_string(), "resolved"),
        (IssueStatus::Open, _) => ("Open".to_string(), "open"),
    }
}

pub struct CodeLine {
    pub number: u32,
    /// The line as highlighted HTML (escaped by the highlighter).
    pub html: String,
    pub highlighted: bool,
}

pub struct CodeView {
    pub label: String,
    pub lines: Vec<CodeLine>,
}

/// The code shown on an issue page: current lines, or the stored snippet when outdated.
#[must_use]
pub fn code(view: &IssueView) -> Option<CodeView> {
    let excerpt = view.code.as_ref()?;
    let anchor = &view.issue.anchor;
    let (label, highlighted, path) = match &view.location {
        Location::Current {
            commit,
            path,
            start_line,
            end_line,
        } if *commit == excerpt.commit => (
            format!("Code @ {}", short_sha(commit)),
            (*start_line, *end_line),
            path.as_str(),
        ),
        _ => (
            format!("Original code @ {}", short_sha(&excerpt.commit)),
            (anchor.start_line, anchor.end_line),
            anchor.path.as_str(),
        ),
    };
    // The excerpt is highlighted on its own; a construct that starts above it, such as a long
    // comment, may be coloured as code.
    let file_name = path.rsplit('/').next().unwrap_or(path);
    let html = highlight::highlight_lines(file_name, &excerpt.lines).lines;
    let lines = (excerpt.first_line..)
        .zip(html)
        .map(|(number, html)| CodeLine {
            number,
            html,
            highlighted: (highlighted.0..=highlighted.1).contains(&number),
        })
        .collect();
    Some(CodeView { label, lines })
}

/// An issue shown inside a code view: its thread, with forms that return to the view.
pub struct ThreadView {
    pub id: i64,
    pub url: String,
    /// The element id, e.g. "issue-12".
    pub anchor: String,
    pub status_label: String,
    pub status_class: &'static str,
    pub resolved: bool,
    pub opened_by: AuthorView,
    pub location: String,
    pub outdated_reason: Option<String>,
    /// The code, for threads shown apart from the lines they are about.
    pub code: Option<CodeView>,
    pub comments: Vec<CommentView>,
    pub comment_action: String,
    pub status_action: String,
    /// The page to return to after a reply or a status change.
    pub back: String,
}

/// An issue's thread for a code view at `back`. `with_code` adds the issue's code, for threads
/// that are not shown under their own lines.
#[must_use]
pub fn thread(view: &IssueView, back: &str, with_code: bool) -> ThreadView {
    let issue = &view.issue;
    let (status_label, status_class) = issue_status(issue.status, issue.resolved_by);
    let url = issue_url(issue.review_id, issue.id);
    ThreadView {
        id: issue.id,
        anchor: format!("issue-{}", issue.id),
        status_label,
        status_class,
        resolved: issue.status == IssueStatus::Resolved,
        opened_by: author(issue.author),
        location: location(view),
        outdated_reason: match &view.location {
            Location::Outdated { reason } => Some(reason.clone()),
            Location::Current { .. } => None,
        },
        code: if with_code { code(view) } else { None },
        comments: view
            .comments
            .iter()
            .map(|item| CommentView {
                back: Some(back.to_string()),
                ..comment(item)
            })
            .collect(),
        comment_action: format!("{url}/comments"),
        status_action: format!("{url}/status"),
        back: back.to_string(),
        url,
    }
}

/// The issue's location in words, e.g. "src/a.rs:42-44 @ a1b2c3d", in the web's plain style
/// rather than the quoted style of MCP output (docs/design.md §12.4). An outdated issue shows
/// where it was opened; the page explains why it is outdated separately.
#[must_use]
pub fn location(view: &IssueView) -> String {
    let anchor = &view.issue.anchor;
    let side = if anchor.side == Side::Base {
        " (base side)"
    } else {
        ""
    };
    let opened_at = format!(
        "{}:{}{side} @ {}",
        anchor.path,
        lines(anchor.start_line, anchor.end_line),
        short_sha(&anchor.commit)
    );
    match &view.location {
        Location::Current {
            commit,
            path,
            start_line,
            end_line,
        } => {
            if *commit == anchor.commit {
                opened_at
            } else if *path == anchor.path && *start_line == anchor.start_line {
                format!("{opened_at}; unchanged at {}", short_sha(commit))
            } else {
                format!(
                    "{path}:{}{side} @ {} (opened at {opened_at})",
                    lines(*start_line, *end_line),
                    short_sha(commit)
                )
            }
        }
        Location::Outdated { .. } => opened_at,
    }
}

/// A line range as "42-44", or "42" for one line.
fn lines(start: u32, end: u32) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}-{end}")
    }
}

pub struct EventView {
    pub when: String,
    pub actor: AuthorView,
    pub text: String,
    pub link: Option<String>,
}

#[must_use]
pub fn event(event: &Event) -> EventView {
    let issue = event.issue_id.map(|id| format!("issue #{id}"));
    let issue = issue.as_deref().unwrap_or("an issue");
    let text = match event.kind {
        EventKind::ReviewCreated => "started the review".to_string(),
        EventKind::ReviewRetitled => "renamed the review".to_string(),
        EventKind::ReviewBaseChanged => "changed the base".to_string(),
        EventKind::ReviewClosed => "closed the review".to_string(),
        EventKind::ReviewReopened => "reopened the review".to_string(),
        EventKind::SummaryUpdated => "edited the summary".to_string(),
        EventKind::IssueCreated => format!("opened {issue}"),
        EventKind::IssueResolved => format!("resolved {issue}"),
        EventKind::IssueReopened => format!("reopened {issue}"),
        EventKind::CommentAdded if event.issue_id.is_some() => format!("commented on {issue}"),
        EventKind::CommentAdded => "commented on the summary".to_string(),
        EventKind::CommentEdited => match event.comment_id {
            Some(id) => format!("edited comment c{id}"),
            None => "edited a comment".to_string(),
        },
    };
    let link = match (event.issue_id, event.comment_id) {
        (Some(issue_id), Some(comment_id)) => Some(format!(
            "{}#c{comment_id}",
            issue_url(event.review_id, issue_id)
        )),
        (Some(issue_id), None) => Some(issue_url(event.review_id, issue_id)),
        (None, Some(comment_id)) => Some(format!("{}#c{comment_id}", review_url(event.review_id))),
        (None, None) => None,
    };
    EventView {
        when: time::to_local(&event.created_at),
        actor: author(event.actor),
        text,
        link,
    }
}

pub struct Link {
    pub label: String,
    pub url: String,
}

/// Links from an issue to its lines in the code views: the Files view for side `head`, and the
/// Changes view when the review has a diff.
#[must_use]
pub fn code_links(view: &IssueView, has_diff: bool) -> Vec<Link> {
    let Location::Current {
        path,
        start_line,
        end_line,
        ..
    } = &view.location
    else {
        return Vec::new();
    };
    let review_id = view.issue.review_id;
    let mut links = Vec::new();
    if view.issue.anchor.side == Side::Head {
        links.push(Link {
            label: "View in file".to_string(),
            url: format!(
                "{}#L{start_line}-L{end_line}",
                files_url(review_id, path, false)
            ),
        });
    }
    if has_diff {
        links.push(Link {
            label: "View in changes".to_string(),
            url: format!("{}#{}", changes_url(review_id), file_anchor(path)),
        });
    }
    links
}

/// The element id of a file's section in the Changes view: `file-` and the path's bytes in hex,
/// which is unambiguous and needs no escaping in a URL fragment.
#[must_use]
pub fn file_anchor(path: &str) -> String {
    let mut anchor = String::from("file-");
    for byte in path.bytes() {
        let _ = write!(anchor, "{byte:02x}");
    }
    anchor
}

pub struct FilterLink {
    pub label: String,
    pub url: String,
    pub active: bool,
}

/// The first non-empty line of a body, shortened for lists.
#[must_use]
pub fn first_line(body: &str) -> String {
    const MAX_CHARS: usize = 100;
    let line = crate::text::split_lines(body)
        .into_iter()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    if line.chars().count() > MAX_CHARS {
        let shortened: String = line.chars().take(MAX_CHARS - 1).collect();
        format!("{shortened}…")
    } else {
        line.to_string()
    }
}

// ── errors ───────────────────────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "message.html")]
struct MessagePage<'a> {
    title: &'a str,
    message: &'a str,
    back: Option<&'a (String, String)>,
    submitted: Option<&'a str>,
}

/// An error shown as a page. It can link back to where the user was and keep the text they
/// submitted, so nothing they typed is lost. Boxed so `Result<_, PageError>` stays small.
#[derive(Debug)]
pub struct PageError(Box<PageErrorDetails>);

#[derive(Debug)]
struct PageErrorDetails {
    status: StatusCode,
    title: String,
    message: String,
    back: Option<(String, String)>,
    submitted: Option<String>,
}

impl PageError {
    #[must_use]
    pub fn new(status: StatusCode, title: &str, message: &str) -> Self {
        Self(Box::new(PageErrorDetails {
            status,
            title: title.to_string(),
            message: message.to_string(),
            back: None,
            submitted: None,
        }))
    }

    /// Add a link back to `url`, labelled `label`.
    #[must_use]
    pub fn back(mut self, url: String, label: &str) -> Self {
        self.0.back = Some((url, label.to_string()));
        self
    }

    /// Show the text the user submitted so they can copy it.
    #[must_use]
    pub fn submitted(mut self, text: &str) -> Self {
        if !text.trim().is_empty() {
            self.0.submitted = Some(text.to_string());
        }
        self
    }
}

impl From<Error> for PageError {
    fn from(error: Error) -> Self {
        let (status, title) = match &error {
            Error::Invalid(_) => (StatusCode::BAD_REQUEST, "Not done"),
            Error::NotFound(_) => (StatusCode::NOT_FOUND, "Not found"),
            Error::Forbidden(_) => (StatusCode::FORBIDDEN, "Not allowed"),
            Error::Conflict(_) => (StatusCode::CONFLICT, "Changed by someone else"),
            Error::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "Unavailable"),
            Error::Internal(_) | Error::Git(_) | Error::Db(_) | Error::Io(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong")
            }
        };
        Self::new(status, title, &error.to_string())
    }
}

impl From<askama::Error> for PageError {
    fn from(error: askama::Error) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Something went wrong",
            &format!("could not render the page: {error}"),
        )
    }
}

impl IntoResponse for PageError {
    fn into_response(self) -> Response {
        let details = &self.0;
        let page = MessagePage {
            title: &details.title,
            message: &details.message,
            back: details.back.as_ref(),
            submitted: details.submitted.as_deref(),
        };
        match page.render() {
            Ok(html) => (details.status, Html(html)).into_response(),
            Err(error) => (
                details.status,
                format!("{}: {} ({error})", details.title, details.message),
            )
                .into_response(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_page_paths() {
        let path = |view, file, line| review_page_path(7, view, file, line);
        assert_eq!(path(View::Overview, None, None).unwrap(), "/reviews/7");
        assert_eq!(path(View::Files, None, None).unwrap(), "/reviews/7/files/");
        assert_eq!(
            path(View::Files, Some("src/a b.rs"), Some(12)).unwrap(),
            "/reviews/7/files/src/a%20b.rs#L12"
        );
        assert_eq!(
            path(View::Changes, None, None).unwrap(),
            "/reviews/7/changes"
        );
        assert_eq!(
            path(View::Changes, Some("a.rs"), Some(3)).unwrap(),
            "/reviews/7/changes#file-612e7273-H3"
        );
        assert!(path(View::Overview, Some("a.rs"), None).is_err());
        assert!(path(View::Files, None, Some(3)).is_err());
        assert!(path(View::Files, Some("../etc/passwd"), None).is_err());
        for valid in [
            path(View::Files, Some("src/a b.rs"), Some(12)).unwrap(),
            path(View::Changes, Some("a.rs"), Some(3)).unwrap(),
        ] {
            assert!(
                crate::web::is_safe_next(&valid),
                "{valid} must survive sign-in"
            );
        }
    }

    #[test]
    fn repo_locations() {
        assert_eq!(repo_location("/srv/repos/api/.git"), "/srv/repos/api");
        assert_eq!(repo_location("/srv/repos/api.git"), "/srv/repos/api.git");
        if let Ok(home) = std::env::var("HOME") {
            assert_eq!(repo_location(&format!("{home}/src/api/.git")), "~/src/api");
        }
    }

    #[test]
    fn first_line_skips_blank_lines_and_truncates() {
        assert_eq!(first_line("\n  Hello  \nworld"), "Hello");
        assert_eq!(first_line(&"x".repeat(150)).chars().count(), 100);
    }

    #[test]
    fn edited_markers_name_the_editor() {
        let mut c = Comment {
            id: 1,
            review_id: 2,
            issue_id: Some(3),
            author: Actor::Reviewer,
            body: "b".into(),
            head_sha: "abcdef0123".into(),
            created_at: "2026-10-07T14:02:11.482Z".into(),
            edited_by: None,
            edited_at: None,
            version: 1,
        };
        assert_eq!(comment(&c).edited, None);
        c.edited_by = Some(Actor::Human);
        assert_eq!(comment(&c).edited.as_deref(), Some("edited by you"));
        c.edited_by = Some(Actor::Reviewer);
        assert_eq!(comment(&c).edited.as_deref(), Some("edited"));
        assert_eq!(comment_url(&c), "/reviews/2/issues/3#c1");
    }
}
