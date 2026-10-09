//! View models for templates, and the page-level error type.
//!
//! Display strings are computed here rather than in templates, so templates stay free of logic
//! and every value they print is auto-escaped (only Markdown output is marked safe).

use std::fmt::Write as _;

use askama::Template;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

use crate::error::Error;
use crate::git::FileChange;
use crate::model::{
    Actor, Anchor, BaseSource, Comment, Event, EventKind, IssueStatus, ReviewStatus, Severity,
    Side, short_ref, short_sha,
};
use crate::service::{CodeExcerpt, IssueView, Location, ReviewView};
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
    /// The page: "overview", "changes" or "files", the tabs; or "issue", which shows a link back
    /// to the overview instead of the tabs. Only the overview shows the hand-off note.
    pub tab: &'static str,
    /// Whose turn the review is; `None` for a closed review or when it is nobody's (§11.5).
    pub turn: Option<TurnView>,
    /// The hand-offs before the one `turn` shows, newest first, or all of them when there is no
    /// `turn`. Only the overview fills it in.
    pub hand_offs: Vec<HandOffView>,
    /// Issues with a question for you, as links.
    pub questions: Vec<QuestionLink>,
    /// The review's live-update stream, and the latest event the page reflects (§13.5).
    pub events_url: String,
    pub events_after: i64,
    /// Whether a newer lani has been installed over this server's executable.
    pub outdated: bool,
}

/// What a review page needs for live updates (§13.5).
pub struct Live {
    /// The review's latest event, read before anything else on the page, so that a change the
    /// page might have missed always has a later id.
    pub events_after: i64,
    /// Whether to show the update banner.
    pub outdated: bool,
}

/// Whose turn a review is, as review pages show it.
pub struct TurnView {
    /// "You", or "Claude · reviewer" / "Claude · implementer".
    pub label: &'static str,
    pub class: &'static str,
    /// Whether it is the human's turn.
    pub yours: bool,
    /// E.g. "handed over by Claude · reviewer · 2026-10-08 10:30 · 2 rounds without you".
    pub detail: Option<String>,
    /// The hand-off note, rendered.
    pub note_html: Option<String>,
}

/// A past change of turn, compactly.
pub struct HandOffView {
    pub when: String,
    pub by: AuthorView,
    /// `None` when the turn was cleared.
    pub to: Option<AuthorView>,
    pub note_html: Option<String>,
}

/// The hand-off history the overview shows under the turn: every change of turn except the
/// newest when the turn box shows that one already. `hand_offs` is newest first.
#[must_use]
pub fn hand_off_history(review: &crate::model::Review, hand_offs: &[Event]) -> Vec<HandOffView> {
    let shown = usize::from(turn(review).is_some());
    hand_offs
        .iter()
        .skip(shown)
        .map(|event| HandOffView {
            when: time::to_local(&event.created_at),
            by: author(event.actor),
            to: event.turn_to.map(author),
            note_html: event.note.as_deref().map(markdown::render),
        })
        .collect()
}

/// An issue with a question for the human.
pub struct QuestionLink {
    pub id: i64,
    pub title: String,
    pub url: String,
    pub asked_by: AuthorView,
}

/// Whose turn an open review is, or `None` when closed or nobody's.
#[must_use]
pub fn turn(review: &crate::model::Review) -> Option<TurnView> {
    if review.status != ReviewStatus::Open {
        return None;
    }
    let holder = review.turn?;
    let who = author(holder);
    let detail = match (review.turn_by, &review.turn_at) {
        (Some(by), Some(at)) => {
            let by = if by == Actor::Human {
                "you"
            } else {
                author(by).label
            };
            let rounds = match review.rounds {
                0 => String::new(),
                1 => " · 1 round without you".to_string(),
                count => format!(" · {count} rounds without you"),
            };
            Some(format!(
                "handed over by {by} · {}{rounds}",
                time::to_local(at)
            ))
        }
        _ => None,
    };
    Some(TurnView {
        label: who.label,
        class: who.class,
        yours: holder == Actor::Human,
        detail,
        note_html: review.turn_note.as_deref().map(markdown::render),
    })
}

/// Links to the issues with a question for the human.
#[must_use]
pub fn question_links(view: &ReviewView) -> Vec<QuestionLink> {
    view.questions
        .iter()
        .map(|question| QuestionLink {
            id: question.issue_id,
            title: question.title.clone(),
            url: issue_url(view.review.id, question.issue_id),
            asked_by: author(question.asked_by),
        })
        .collect()
}

/// What an issue's question for the human says, while it is open: e.g. "Claude · implementer
/// asked you about this issue · 2026-10-08 10:30".
#[must_use]
pub fn question_notice(issue: &crate::model::Issue) -> Option<String> {
    let (asker, at) = (issue.asked_by?, issue.asked_at.as_deref()?);
    Some(format!(
        "{} asked you about this issue · {}",
        author(asker).label,
        time::to_local(at)
    ))
}

#[must_use]
pub fn review_header(view: &ReviewView, live: &Live) -> ReviewHeader {
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
        turn: turn(review),
        hand_offs: Vec::new(),
        questions: if review.status == ReviewStatus::Open {
            question_links(view)
        } else {
            Vec::new()
        },
        events_url: format!("{}/events", review_url(review.id)),
        events_after: live.events_after,
        outdated: live.outdated,
    }
}

/// One row in a review's issue list.
pub struct IssueRow {
    pub id: i64,
    pub url: String,
    pub status_label: String,
    pub status_class: &'static str,
    /// The first span, and how many more there are.
    pub location: String,
    /// "Outdated" when every span is, "Partly outdated" when some are.
    pub outdated: Option<&'static str>,
    pub title: String,
    pub severity: SeverityView,
    pub comments: String,
    pub last_author: Option<AuthorView>,
    /// Whether a question about this issue is waiting for the human.
    pub needs_you: bool,
    /// Which roles have created or commented on the issue: reviewer, human, implementer.
    pub lights: [RoleLight; 3],
}

/// One light in an issue row: whether a role has created or commented on the issue.
pub struct RoleLight {
    /// The role's author class, giving the light its colour.
    pub class: &'static str,
    pub lit: bool,
    /// E.g. "Claude · reviewer: created this issue and added 2 comments".
    pub tooltip: String,
}

#[must_use]
pub fn issue_row(view: &IssueView) -> IssueRow {
    let issue = &view.issue;
    let mut location = view
        .spans()
        .next()
        .map(|(anchor, location)| {
            let side = if anchor.side == Side::Base {
                " (base side)"
            } else {
                ""
            };
            match location {
                Location::Current {
                    path,
                    start_line,
                    end_line,
                    ..
                } => format!("{path}:{}{side}", lines(*start_line, *end_line)),
                Location::Outdated { .. } => format!(
                    "{}:{}{side}",
                    anchor.path,
                    lines(anchor.start_line, anchor.end_line)
                ),
            }
        })
        .unwrap_or_default();
    if view.locations.len() > 1 {
        let _ = write!(location, " + {} more", view.locations.len() - 1);
    }
    let outdated_spans = view
        .locations
        .iter()
        .filter(|location| matches!(location, Location::Outdated { .. }))
        .count();
    let outdated = match outdated_spans {
        0 => None,
        count if count == view.locations.len() => Some("Outdated"),
        _ => Some("Partly outdated"),
    };
    let (status_label, status_class) = issue_status(issue.status, issue.resolved_by);
    IssueRow {
        id: issue.id,
        url: issue_url(issue.review_id, issue.id),
        status_label,
        status_class,
        location,
        outdated,
        title: issue.title.clone(),
        severity: severity(issue.severity),
        needs_you: issue.asked_by.is_some(),
        comments: match view.comments.len() {
            1 => "1 comment".to_string(),
            count => format!("{count} comments"),
        },
        last_author: view.comments.last().map(|comment| author(comment.author)),
        lights: role_lights(view),
    }
}

fn role_lights(view: &IssueView) -> [RoleLight; 3] {
    [Actor::Reviewer, Actor::Human, Actor::Implementer].map(|role| {
        let created = view.issue.author == role;
        // The first comment is the issue's description, written by its creator.
        let comments = view
            .comments
            .iter()
            .skip(1)
            .filter(|comment| comment.author == role)
            .count();
        let added = match comments {
            1 => "added 1 comment".to_string(),
            count => format!("added {count} comments"),
        };
        let status = match (created, comments) {
            (true, 0) => "created this issue".to_string(),
            (true, _) => format!("created this issue and {added}"),
            (false, 0) => "no comments yet".to_string(),
            (false, _) => added,
        };
        let who = author(role);
        RoleLight {
            class: who.class,
            lit: created || comments > 0,
            tooltip: format!("{}: {status}", who.label),
        }
    })
}

/// How a severity is shown: its name, and a CSS class for its badge colour.
#[derive(Clone, Copy, Debug)]
pub struct SeverityView {
    pub label: &'static str,
    pub class: &'static str,
}

#[must_use]
pub fn severity(severity: Severity) -> SeverityView {
    SeverityView {
        label: severity.as_str(),
        class: match severity {
            Severity::High => "sev-high",
            Severity::Medium => "sev-medium",
            Severity::Low => "sev-low",
            Severity::Nit => "sev-nit",
        },
    }
}

/// One choice in a severity picker.
pub struct SeverityOption {
    pub value: &'static str,
    pub selected: bool,
}

/// Every severity, most severe first, with `current` selected (none when `None`).
#[must_use]
pub fn severity_options(current: Option<Severity>) -> Vec<SeverityOption> {
    [
        Severity::High,
        Severity::Medium,
        Severity::Low,
        Severity::Nit,
    ]
    .into_iter()
    .map(|option| SeverityOption {
        value: option.as_str(),
        selected: current == Some(option),
    })
    .collect()
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

/// One span of an issue, as the issue page and threads show it.
pub struct SpanView {
    /// Where the span is, e.g. "src/a.rs:42-44 @ a1b2c3d".
    pub location: String,
    /// Why the span is outdated, if it is.
    pub outdated_reason: Option<String>,
    /// The span's code, when shown.
    pub code: Option<CodeView>,
    /// The span's lines in the Files and Changes views.
    pub links: Vec<Link>,
}

/// The spans of an issue. `with_code` adds each span's code (the view must have been loaded with
/// code); `changes`, the review's file changes, gives the spans they show links to the Changes
/// view.
#[must_use]
pub fn span_views(view: &IssueView, with_code: bool, changes: &[FileChange]) -> Vec<SpanView> {
    let codes = view.code.as_deref().unwrap_or_default();
    let numbered = view.locations.len() > 1;
    (1..)
        .zip(view.spans())
        .map(|(number, (anchor, location))| SpanView {
            location: span_location(anchor, location),
            outdated_reason: match location {
                Location::Outdated { reason } => Some(reason.clone()),
                Location::Current { .. } => None,
            },
            code: codes
                .get(number - 1)
                .filter(|_| with_code)
                .map(|excerpt| span_code(anchor, location, excerpt, numbered.then_some(number))),
            links: span_links(view.issue.review_id, anchor, location, changes),
        })
        .collect()
}

/// A span's code: current lines, or its stored snippet when outdated. `number` labels the span
/// when the issue has several.
fn span_code(
    anchor: &Anchor,
    location: &Location,
    excerpt: &CodeExcerpt,
    number: Option<usize>,
) -> CodeView {
    let (label, highlighted, path) = match location {
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
    let label = match number {
        Some(number) => format!("Span {number} · {label}"),
        None => label,
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
    CodeView { label, lines }
}

/// An issue shown inside a code view: its thread, with forms that return to the view.
pub struct ThreadView {
    pub id: i64,
    pub title: String,
    pub severity: SeverityView,
    pub url: String,
    /// The element id, e.g. "issue-12".
    pub anchor: String,
    pub status_label: String,
    pub status_class: &'static str,
    pub resolved: bool,
    pub opened_by: AuthorView,
    /// A question about the issue waiting for the human.
    pub question: Option<String>,
    /// Every span, with code for threads shown apart from the lines they are about.
    pub spans: Vec<SpanView>,
    pub comments: Vec<CommentView>,
    pub comment_action: String,
    pub status_action: String,
    /// The page to return to after a reply or a status change.
    pub back: String,
}

/// An issue's thread for a code view at `back`. `with_code` adds each span's code, for threads
/// that are not shown under their own lines; `changes` is as for [`span_views`].
#[must_use]
pub fn thread(view: &IssueView, back: &str, with_code: bool, changes: &[FileChange]) -> ThreadView {
    let issue = &view.issue;
    let (status_label, status_class) = issue_status(issue.status, issue.resolved_by);
    let url = issue_url(issue.review_id, issue.id);
    ThreadView {
        id: issue.id,
        title: issue.title.clone(),
        severity: severity(issue.severity),
        anchor: format!("issue-{}", issue.id),
        status_label,
        status_class,
        resolved: issue.status == IssueStatus::Resolved,
        opened_by: author(issue.author),
        question: question_notice(issue),
        spans: span_views(view, with_code, changes),
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

/// A small mark on a span's last line that points to the issue's thread elsewhere on the page.
pub struct MarkerView {
    pub id: i64,
    pub url: String,
    /// Shown on hover: the issue's number and title.
    pub tooltip: String,
}

#[must_use]
pub fn marker(view: &IssueView) -> MarkerView {
    let issue = &view.issue;
    MarkerView {
        id: issue.id,
        url: format!("#issue-{}", issue.id),
        tooltip: format!(
            "Issue #{}: {} (also about these lines)",
            issue.id, issue.title
        ),
    }
}

/// Where a span is, e.g. "src/a.rs:42-44 @ a1b2c3d", in the web's plain style rather than the
/// quoted style of MCP output (docs/design.md §12.4). An outdated span shows where it was
/// opened; the page says why it is outdated separately.
#[must_use]
pub fn span_location(anchor: &Anchor, location: &Location) -> String {
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
    match location {
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
        EventKind::IssueEdited => format!("changed the title or severity of {issue}"),
        EventKind::IssueResolved => format!("resolved {issue}"),
        EventKind::IssueReopened => format!("reopened {issue}"),
        EventKind::CommentAdded if event.issue_id.is_some() => format!("commented on {issue}"),
        EventKind::CommentAdded => "commented on the summary".to_string(),
        EventKind::TurnChanged => match event.turn_to {
            Some(Actor::Human) => "handed the review to you".to_string(),
            Some(role) => format!("handed the review to {}", author(role).label),
            None => "cleared the turn".to_string(),
        },
        EventKind::HumanAsked => format!("asked you about {issue}"),
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

/// Links from a span to its lines in the code views: the Files view for side `head`, and the
/// section of the Changes view that shows the span's file, if one of `changes` does.
fn span_links(
    review_id: i64,
    anchor: &Anchor,
    location: &Location,
    changes: &[FileChange],
) -> Vec<Link> {
    let Location::Current {
        path,
        start_line,
        end_line,
        ..
    } = location
    else {
        return Vec::new();
    };
    let mut links = Vec::new();
    if anchor.side == Side::Head {
        links.push(Link {
            label: "View in file".to_string(),
            url: format!(
                "{}#L{start_line}-L{end_line}",
                files_url(review_id, path, false)
            ),
        });
    }
    // The change with the span's path on the span's side. Its section is named by its new
    // path, so a base-side span in a renamed file links to the new path's section.
    let change = changes.iter().find(|change| {
        let version = match anchor.side {
            Side::Head => &change.new,
            Side::Base => &change.old,
        };
        version
            .as_ref()
            .is_some_and(|version| version.path == *path)
    });
    if let Some(change) = change {
        links.push(Link {
            label: "View in changes".to_string(),
            url: format!("{}#{}", changes_url(review_id), file_anchor(change.path())),
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
            Error::Outdated(_) => (StatusCode::SERVICE_UNAVAILABLE, "Restart needed"),
            Error::Internal(_) | Error::Git(_) | Error::Db(_) | Error::Io(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong")
            }
        };
        let message = match &error {
            Error::Outdated(message) => outdated_server_message(message),
            _ => error.to_string(),
        };
        Self::new(status, title, &message)
    }
}

/// What the web UI shows once a newer lani has upgraded the database (§7.4).
#[must_use]
pub fn outdated_server_message(message: &str) -> String {
    format!("This web server is out of date: {message}. Restart it: run lani stop, then lani open.")
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
