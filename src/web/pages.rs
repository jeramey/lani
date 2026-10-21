//! Page handlers (GET), plus the health check and the stylesheet.

use std::time::Instant;

use askama::Template;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect};
use serde::Deserialize;

use super::AppState;
use super::activity::{self, ActivityView};
use super::views::{
    self, AuthorView, CommentView, FilterLink, IssueRow, PageError, ReviewHeader, SeverityOption,
    SeverityView, SpanView, author, review_url,
};
use crate::daemon::{self, Health};
use crate::model::{IssueStatus, PromptKind, ReviewStatus, short_ref};
use crate::service::{PromptNotice, ReviewView, ReviewerPrompt};
use crate::settings;
use crate::time;

#[derive(Deserialize)]
pub struct StatusQuery {
    status: Option<String>,
}

/// The overview's query: the issue filter, and the page of the Activity section (§13.3).
#[derive(Deserialize)]
pub struct ReviewQuery {
    status: Option<String>,
    /// Events per page: 10, 25, 50, 100 or `all`.
    events: Option<String>,
    /// The page of events, from 1, the newest.
    page: Option<String>,
    /// The event the pages are anchored at.
    latest: Option<String>,
}

// ── review list ──────────────────────────────────────────────────────────────────────────────

struct ReviewRow {
    url: String,
    id: i64,
    title: String,
    status_label: &'static str,
    status_class: &'static str,
    refs: String,
    open_issues: u64,
    resolved_issues: u64,
    created_by: AuthorView,
    created: String,
    /// It is the human's turn, or a question is waiting for them.
    needs_you: bool,
    turn: Option<views::TurnView>,
}

struct RepoGroup {
    repo_id: i64,
    name: String,
    /// Where the repository lives, so repositories with the same directory name are told apart.
    location: String,
    reviews: Vec<ReviewRow>,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexPage {
    /// Whether to show the update banner (§13.5).
    outdated: bool,
    /// How to restart the server, for the update banner.
    restart: String,
    filters: Vec<FilterLink>,
    groups: Vec<RepoGroup>,
    empty_message: &'static str,
}

/// `GET /`: reviews across all repos, grouped by repo; open ones by default.
pub async fn index(
    State(state): State<AppState>,
    Query(query): Query<StatusQuery>,
) -> Result<Html<String>, PageError> {
    let (status, active, empty_message) = match query.status.as_deref() {
        None | Some("open") => (
            Some(ReviewStatus::Open),
            "open",
            "No open reviews. Ask an agent session to start one with “Start a lani review of this \
             branch.”",
        ),
        Some("closed") => (Some(ReviewStatus::Closed), "closed", "No closed reviews."),
        Some("all") => (None, "all", "No reviews yet."),
        Some(other) => return Err(bad_filter(other)),
    };
    let list = state
        .call(move |service| service.list_reviews(status, None))
        .await?;

    let mut groups: Vec<RepoGroup> = Vec::new();
    for item in &list.items {
        let review = &item.view.review;
        let open = review.status == ReviewStatus::Open;
        let refs = match &review.base_ref {
            Some(base) => format!("{} → {}", short_ref(&review.head_ref), short_ref(base)),
            None => format!("{} (whole tree)", short_ref(&review.head_ref)),
        };
        let row = ReviewRow {
            url: review_url(review.id),
            id: review.id,
            title: review.title.clone(),
            status_label: if open { "Open" } else { "Closed" },
            status_class: if open { "open" } else { "closed" },
            refs,
            open_issues: item.view.counts.open,
            resolved_issues: item.view.counts.resolved,
            created_by: author(review.created_by),
            created: time::to_local(&review.created_at),
            needs_you: open
                && (review.turn == Some(crate::model::Actor::Human)
                    || !item.view.questions.is_empty()),
            turn: views::turn(review),
        };
        let repo = &item.view.repo;
        match groups.iter_mut().find(|group| group.repo_id == repo.id) {
            Some(group) => group.reviews.push(row),
            None => groups.push(RepoGroup {
                repo_id: repo.id,
                name: repo.name.clone(),
                location: views::repo_location(&repo.git_dir),
                reviews: vec![row],
            }),
        }
    }
    let filters = [("Open", "open"), ("Closed", "closed"), ("All", "all")]
        .into_iter()
        .map(|(label, value)| FilterLink {
            label: label.to_string(),
            url: format!("/?status={value}"),
            active: value == active,
        })
        .collect();
    Ok(Html(
        IndexPage {
            outdated: state.outdated(),
            restart: state.instance().endpoint.restart_hint(),
            filters,
            groups,
            empty_message,
        }
        .render()?,
    ))
}

// ── review overview ──────────────────────────────────────────────────────────────────────────

struct BaseOption {
    value: String,
    label: String,
    selected: bool,
}

#[derive(Template)]
#[template(path = "review.html")]
struct ReviewPage {
    review: ReviewHeader,
    editable: bool,
    summary_html: Option<String>,
    summary_text: String,
    summary_version: i64,
    summary_edited: Option<String>,
    summary_comments: Vec<CommentView>,
    issue_filters: Vec<FilterLink>,
    issues: Vec<IssueRow>,
    issues_empty: &'static str,
    base_options: Vec<BaseOption>,
    /// The role chosen in advance in the Hand over form: `reviewer` or `implementer`.
    hand_over_to: &'static str,
    /// The Reviewer prompt panel's prompts; empty for a closed review.
    prompts: Vec<PromptPanel>,
    activity: ActivityView,
}

/// A prompt in the Reviewer prompt panel (§13.9).
struct PromptPanel {
    /// The kind as the save action takes it, such as `claude_code`.
    kind: &'static str,
    label: &'static str,
    text: String,
    default: String,
    edited: bool,
    /// Why the panel shows the default although the human's last copy was an edit.
    notice: Option<&'static str>,
}

impl PromptPanel {
    fn new(prompt: ReviewerPrompt) -> Self {
        Self {
            kind: prompt.kind.as_str(),
            label: match prompt.kind {
                PromptKind::ClaudeCode => "Claude Code",
                PromptKind::Codex => "Codex",
                PromptKind::Generic => "Other harness",
            },
            edited: prompt.edited(),
            notice: prompt.notice.map(|notice| match notice {
                PromptNotice::DefaultChanged => {
                    "The default changed since your last edit, with a new base or a new version of \
                     LANI. Your edit is kept in the history; copying the new default accepts it."
                }
                PromptNotice::Unreadable => {
                    "Your last edit could not be read back from the history, so this is the \
                     default. The edit is still stored; copying starts again from this text."
                }
            }),
            text: prompt.text,
            default: prompt.default,
        }
    }
}

/// `GET /reviews/{id}`: summary, conversation, issues and activity.
pub async fn review(
    State(state): State<AppState>,
    Path(review_id): Path<i64>,
    Query(query): Query<ReviewQuery>,
) -> Result<Html<String>, PageError> {
    let request = activity::request(
        query.events.as_deref(),
        query.page.as_deref(),
        query.latest.as_deref(),
    )?;
    let (status, active, issues_empty) = match query.status.as_deref() {
        None | Some("open") => (Some(IssueStatus::Open), "open", "No open issues."),
        Some("resolved") => (
            Some(IssueStatus::Resolved),
            "resolved",
            "No resolved issues.",
        ),
        Some("all") => (None, "all", "No issues yet."),
        Some(other) => return Err(bad_filter(other)),
    };
    let (events_after, detail, issues, activity, candidates, prompts) = state
        .call(move |service| {
            let events_after = service.latest_event_id(review_id)?;
            let detail = service.get_review(review_id, None)?;
            let issues = service.list_issues(review_id, status, None, false)?;
            let activity = service.activity(review_id, request)?;
            let open = detail.view.review.status == ReviewStatus::Open;
            let candidates = if open && detail.view.repo_unavailable.is_none() {
                service.base_candidates(review_id)?
            } else {
                Vec::new()
            };
            let prompts = if open {
                service.reviewer_prompts(review_id)?
            } else {
                Vec::new()
            };
            Ok((events_after, detail, issues, activity, candidates, prompts))
        })
        .await?;

    let review = &detail.view.review;
    let counts = detail.view.counts;
    let issue_filters = [
        (format!("Open ({})", counts.open), "open"),
        (format!("Resolved ({})", counts.resolved), "resolved"),
        (format!("All ({})", counts.open + counts.resolved), "all"),
    ]
    .into_iter()
    .map(|(label, value)| FilterLink {
        label,
        url: activity::overview_url(review_id, value, request, "issues"),
        active: value == active,
    })
    .collect();
    let base_options = base_options(&detail.view, candidates);
    let summary_edited = match (review.summary_updated_by, &review.summary_updated_at) {
        (Some(editor), Some(at)) => Some(format!(
            "Last edited by {} · {}",
            author(editor).phrase,
            time::to_local(at)
        )),
        _ => None,
    };
    let live = state.live(events_after);
    let mut header = views::review_header(&detail.view, &live);
    header.hand_offs = views::hand_off_history(review, &detail.hand_offs);
    let page = ReviewPage {
        review: header,
        editable: review.status == ReviewStatus::Open,
        summary_html: (!review.summary.trim().is_empty())
            .then(|| crate::markdown::render(&review.summary)),
        summary_text: review.summary.clone(),
        summary_version: review.summary_version,
        summary_edited,
        summary_comments: detail.summary_comments.iter().map(views::comment).collect(),
        issue_filters,
        issues: {
            // Issues waiting for the human come first, so they are easy to find.
            let mut rows: Vec<IssueRow> = issues.issues.iter().map(views::issue_row).collect();
            rows.sort_by_key(|row| !row.needs_you);
            rows
        },
        issues_empty,
        base_options,
        hand_over_to: views::likely_next(review).as_str(),
        prompts: prompts.into_iter().map(PromptPanel::new).collect(),
        activity: activity::view(review_id, active, request, &activity),
    };
    Ok(Html(page.render()?))
}

// ── issue ────────────────────────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "issue.html")]
struct IssuePage {
    review: ReviewHeader,
    editable: bool,
    id: i64,
    title: String,
    severity: SeverityView,
    /// E.g. "title or severity edited by you".
    details_edited: Option<String>,
    version: i64,
    severity_options: Vec<SeverityOption>,
    details_action: String,
    status_label: String,
    status_class: &'static str,
    resolved: bool,
    opened_by: AuthorView,
    opened: String,
    question: Option<String>,
    spans: Vec<SpanView>,
    comments: Vec<CommentView>,
    status_action: String,
    comment_action: String,
}

/// The base picker's options: the branch candidates, with the review's base selected.
fn base_options(view: &ReviewView, candidates: Vec<String>) -> Vec<BaseOption> {
    let review = &view.review;
    let mut base_options: Vec<BaseOption> = candidates
        .into_iter()
        .map(|name| BaseOption {
            label: short_ref(&name).to_string(),
            selected: review.base_ref.as_deref() == Some(name.as_str()),
            value: name,
        })
        .collect();
    // A base that is not a branch candidate (a tag, or a branch that was deleted) still has to be
    // shown as selected; otherwise the browser shows the first option, "None", and saving
    // without looking would turn the review into a whole-tree review.
    if let Some(base) = &review.base_ref
        && !base_options.iter().any(|option| option.selected)
    {
        let missing = view.git.as_ref().is_some_and(|git| git.base_sha.is_none());
        let note = if missing {
            "current; ref no longer exists"
        } else {
            "current"
        };
        base_options.insert(
            0,
            BaseOption {
                label: format!("{} ({note})", short_ref(base)),
                value: base.clone(),
                selected: true,
            },
        );
    }
    base_options
}

/// `GET /reviews/{review_id}/issues/{issue_id}`: the issue's code and thread.
pub async fn issue(
    State(state): State<AppState>,
    Path((review_id, issue_id)): Path<(i64, i64)>,
) -> Result<Html<String>, PageError> {
    let (events_after, detail, view, changes) = state
        .call(move |service| {
            let events_after = service.latest_event_id(review_id)?;
            let view = service.get_issue(issue_id)?;
            if view.issue.review_id != review_id {
                return Err(crate::error::Error::NotFound(format!(
                    "issue {issue_id} is not part of review {review_id}"
                )));
            }
            let detail = service.get_review(review_id, None)?;
            let changes = crate::service::review_changes(&detail.view)?;
            Ok((events_after, detail, view, changes))
        })
        .await?;
    let issue = &view.issue;
    let (status_label, status_class) = views::issue_status(issue.status, issue.resolved_by);
    let live = state.live(events_after);
    let page = IssuePage {
        review: views::ReviewHeader {
            tab: "issue",
            ..views::review_header(&detail.view, &live)
        },
        editable: detail.view.review.status == ReviewStatus::Open,
        id: issue.id,
        title: issue.title.clone(),
        severity: views::severity(issue.severity),
        details_edited: issue
            .edited_by
            .map(|editor| format!("title or severity edited by {}", author(editor).phrase)),
        version: issue.version,
        severity_options: views::severity_options(Some(issue.severity)),
        details_action: format!("{}/details", views::issue_url(review_id, issue_id)),
        status_label,
        status_class,
        resolved: issue.status == IssueStatus::Resolved,
        opened_by: author(issue.author),
        opened: time::to_local(&issue.created_at),
        question: views::question_notice(issue),
        spans: views::span_views(&view, true, &changes),
        comments: view.comments.iter().map(views::comment).collect(),
        status_action: format!("{}/status", views::issue_url(review_id, issue_id)),
        comment_action: format!("{}/comments", views::issue_url(review_id, issue_id)),
    };
    Ok(Html(page.render()?))
}

/// `GET /issues/{issue_id}`: redirect to the issue's page under its review.
pub async fn issue_redirect(
    State(state): State<AppState>,
    Path(issue_id): Path<i64>,
) -> Result<Redirect, PageError> {
    let issue = state
        .call(move |service| service.issue_record(issue_id))
        .await?;
    Ok(Redirect::to(&views::issue_url(issue.review_id, issue.id)))
}

// ── settings and server ──────────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsPage {
    /// Whether to show the update banner (§13.5).
    outdated: bool,
    saved: String,
    effective: String,
    source: &'static str,
    remaining: Option<String>,
    version: &'static str,
    pid: u32,
    /// `port 5264`, or `socket /…/run/web.sock` in socket mode.
    endpoint: String,
    /// How to get LANI back once it has stopped itself.
    reopen: String,
    /// How to restart the server, for the update banner.
    restart: String,
    started: String,
    round_limit: u32,
}

/// `GET /settings`: the idle timeout and facts about the running server.
pub async fn settings(State(state): State<AppState>) -> Result<Html<String>, PageError> {
    let (saved, round_limit) = state
        .call(|service| Ok((service.idle_timeout()?, service.round_limit()?)))
        .await?;
    let idle = state.idle();
    let instance = state.instance();
    let page = SettingsPage {
        outdated: state.outdated(),
        saved: saved.to_string(),
        effective: idle.timeout.to_string(),
        source: idle.source.label(),
        remaining: idle.remaining(Instant::now()).map(settings::hours_minutes),
        version: daemon::VERSION,
        pid: instance.pid,
        endpoint: instance.endpoint.describe(),
        reopen: instance.endpoint.open_hint(),
        restart: instance.endpoint.restart_hint(),
        started: time::to_local(&instance.started_at),
        round_limit,
    };
    Ok(Html(page.render()?))
}

/// `GET /api/health`: identifies this server instance to `lani open`, `status` and `stop`.
pub async fn health(State(state): State<AppState>) -> Json<Health> {
    let instance = state.instance();
    let idle = state.idle();
    Json(Health {
        instance_id: instance.instance_id.clone(),
        version: daemon::VERSION.to_string(),
        pid: instance.pid,
        endpoint: instance.endpoint.clone(),
        started_at: instance.started_at.clone(),
        idle_timeout: idle.timeout.to_string(),
        idle_timeout_source: idle.source.label().to_string(),
        idle_remaining_secs: idle.remaining(Instant::now()).map(|left| left.as_secs()),
    })
}

pub async fn stylesheet() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("static/lani.css"),
    )
}

/// `GET /static/lani.js`: line selection and the new-issue form in the code views (§13.4).
pub async fn script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("static/lani.js"),
    )
}

/// `GET /static/highlight.css`: colours for highlighted code (§13.6).
pub async fn highlight_stylesheet() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        crate::highlight::theme_css(),
    )
}

pub async fn not_found() -> PageError {
    PageError::new(
        StatusCode::NOT_FOUND,
        "Not found",
        "There is no page at this address.",
    )
    .back("/".to_string(), "All reviews")
}

fn bad_filter(value: &str) -> PageError {
    PageError::new(
        StatusCode::BAD_REQUEST,
        "Unknown filter",
        &format!("{value:?} is not a status filter."),
    )
}
