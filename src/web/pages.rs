//! Page handlers (GET), plus the health check and the stylesheet.

use std::time::Instant;

use askama::Template;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect};
use serde::Deserialize;

use super::AppState;
use super::views::{
    self, AuthorView, CodeView, CommentView, EventView, FilterLink, IssueRow, PageError,
    ReviewHeader, author, review_url,
};
use crate::daemon::{self, Health};
use crate::model::{IssueStatus, ReviewStatus, short_ref};
use crate::service::Location;
use crate::settings;
use crate::time;

#[derive(Deserialize)]
pub struct StatusQuery {
    status: Option<String>,
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
}

struct RepoGroup {
    repo: String,
    reviews: Vec<ReviewRow>,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexPage {
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
            "No open reviews. Ask Claude to start one with “Start a lani review of this branch.”",
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
        };
        match groups
            .iter_mut()
            .find(|group| group.repo == item.view.repo.name)
        {
            Some(group) => group.reviews.push(row),
            None => groups.push(RepoGroup {
                repo: item.view.repo.name.clone(),
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
    events: Vec<EventView>,
}

/// `GET /reviews/{id}`: summary, conversation, issues and activity.
pub async fn review(
    State(state): State<AppState>,
    Path(review_id): Path<i64>,
    Query(query): Query<StatusQuery>,
) -> Result<Html<String>, PageError> {
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
    let (detail, issues, events, candidates) = state
        .call(move |service| {
            let detail = service.get_review(review_id, None)?;
            let issues = service.list_issues(review_id, status, None, false)?;
            let events = service.list_events(review_id)?;
            let candidates = if detail.view.review.status == ReviewStatus::Open
                && detail.view.repo_unavailable.is_none()
            {
                service.base_candidates(review_id)?
            } else {
                Vec::new()
            };
            Ok((detail, issues, events, candidates))
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
        url: format!("{}?status={value}#issues", review_url(review_id)),
        active: value == active,
    })
    .collect();
    let base_options = candidates
        .into_iter()
        .map(|name| BaseOption {
            label: short_ref(&name).to_string(),
            selected: review.base_ref.as_deref() == Some(name.as_str()),
            value: name,
        })
        .collect();
    let summary_edited = match (review.summary_updated_by, &review.summary_updated_at) {
        (Some(editor), Some(at)) => Some(format!(
            "Last edited by {} · {}",
            author(editor).label,
            time::to_local(at)
        )),
        _ => None,
    };
    let page = ReviewPage {
        review: views::review_header(&detail.view),
        editable: review.status == ReviewStatus::Open,
        summary_html: (!review.summary.trim().is_empty())
            .then(|| crate::markdown::render(&review.summary)),
        summary_text: review.summary.clone(),
        summary_version: review.summary_version,
        summary_edited,
        summary_comments: detail.summary_comments.iter().map(views::comment).collect(),
        issue_filters,
        issues: issues.issues.iter().map(views::issue_row).collect(),
        issues_empty,
        base_options,
        events: events.iter().map(views::event).collect(),
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
    status_label: String,
    status_class: &'static str,
    resolved: bool,
    opened_by: AuthorView,
    opened: String,
    location: String,
    outdated_reason: Option<String>,
    code: Option<CodeView>,
    comments: Vec<CommentView>,
    status_action: String,
    comment_action: String,
}

/// `GET /reviews/{review_id}/issues/{issue_id}`: the issue's code and thread.
pub async fn issue(
    State(state): State<AppState>,
    Path((review_id, issue_id)): Path<(i64, i64)>,
) -> Result<Html<String>, PageError> {
    let (detail, view) = state
        .call(move |service| {
            let view = service.get_issue(issue_id)?;
            if view.issue.review_id != review_id {
                return Err(crate::error::Error::NotFound(format!(
                    "issue {issue_id} is not part of review {review_id}"
                )));
            }
            Ok((service.get_review(review_id, None)?, view))
        })
        .await?;
    let issue = &view.issue;
    let (status_label, status_class) = views::issue_status(issue.status, issue.resolved_by);
    let outdated_reason = match &view.location {
        Location::Outdated { reason } => Some(reason.clone()),
        Location::Current { .. } => None,
    };
    let page = IssuePage {
        review: views::review_header(&detail.view),
        editable: detail.view.review.status == ReviewStatus::Open,
        id: issue.id,
        status_label,
        status_class,
        resolved: issue.status == IssueStatus::Resolved,
        opened_by: author(issue.author),
        opened: time::to_local(&issue.created_at),
        location: views::location(&view),
        outdated_reason,
        code: views::code(&view),
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
    saved: String,
    effective: String,
    source: &'static str,
    remaining: Option<String>,
    version: &'static str,
    pid: u32,
    port: u16,
    started: String,
}

/// `GET /settings`: the idle timeout and facts about the running server.
pub async fn settings(State(state): State<AppState>) -> Result<Html<String>, PageError> {
    let saved = state.call(|service| service.idle_timeout()).await?;
    let idle = state.idle();
    let instance = state.instance();
    let page = SettingsPage {
        saved: saved.to_string(),
        effective: idle.timeout.to_string(),
        source: idle.source.label(),
        remaining: idle.remaining(Instant::now()).map(settings::hours_minutes),
        version: daemon::VERSION,
        pid: instance.pid,
        port: instance.port,
        started: time::to_local(&instance.started_at),
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
        port: instance.port,
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
