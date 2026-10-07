//! View models for templates, and the page-level error type.
//!
//! Display strings are computed here rather than in templates, so templates stay free of logic
//! and every value they print is auto-escaped (only Markdown output is marked safe).

use std::fmt::Write as _;

use askama::Template;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

use crate::error::Error;
use crate::mcp::render::location_text;
use crate::model::{
    Actor, BaseSource, Comment, Event, EventKind, IssueStatus, ReviewStatus, Side, short_ref,
    short_sha,
};
use crate::service::{IssueView, Location, ReviewView};
use crate::{markdown, time};

// ── URLs ─────────────────────────────────────────────────────────────────────────────────────

#[must_use]
pub fn review_url(review_id: i64) -> String {
    format!("/reviews/{review_id}")
}

#[must_use]
pub fn issue_url(review_id: i64, issue_id: i64) -> String {
    format!("/reviews/{review_id}/issues/{issue_id}")
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
    let lines = |start: u32, end: u32| {
        if start == end {
            start.to_string()
        } else {
            format!("{start}-{end}")
        }
    };
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
    pub text: String,
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
    let (label, highlighted) = match &view.location {
        Location::Current {
            commit,
            start_line,
            end_line,
            ..
        } if *commit == excerpt.commit => (
            format!("Code @ {}", short_sha(commit)),
            (*start_line, *end_line),
        ),
        _ => (
            format!("Original code @ {}", short_sha(&excerpt.commit)),
            (anchor.start_line, anchor.end_line),
        ),
    };
    let lines = (excerpt.first_line..)
        .zip(&excerpt.lines)
        .map(|(number, text)| CodeLine {
            number,
            text: text.clone(),
            highlighted: (highlighted.0..=highlighted.1).contains(&number),
        })
        .collect();
    Some(CodeView { label, lines })
}

/// The issue's location in words, e.g. "src/a.rs:42-44 (head side) @ a1b2c3d".
#[must_use]
pub fn location(view: &IssueView) -> String {
    location_text(view)
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

pub struct FilterLink {
    pub label: String,
    pub url: String,
    pub active: bool,
}

/// The first non-empty line of a body, shortened for lists.
#[must_use]
pub fn first_line(body: &str) -> String {
    const MAX_CHARS: usize = 100;
    let line = body
        .lines()
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
