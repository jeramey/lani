//! Form actions (POST). Each one redirects back to the page it came from (303 See Other), or
//! shows an error page that keeps the submitted text. Every action acts as the human.

use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::views::{PageError, author, comment_url, issue_url, review_url};
use super::{AppState, TimeoutSource};
use crate::base::BaseMode;
use crate::error::Error;
use crate::markdown;
use crate::model::{Actor, IssueStatus};
use crate::service::CommentTarget;
use crate::settings::IdleTimeout;

#[derive(Deserialize)]
pub struct TitleForm {
    title: String,
}

#[derive(Deserialize)]
pub struct BaseForm {
    /// A full ref name, or empty for a whole-tree review.
    base: String,
}

#[derive(Deserialize)]
pub struct SummaryForm {
    summary: String,
    version: i64,
}

#[derive(Deserialize)]
pub struct BodyForm {
    body: String,
}

#[derive(Deserialize)]
pub struct StatusForm {
    status: String,
    #[serde(default)]
    comment: String,
}

#[derive(Deserialize)]
pub struct EditForm {
    body: String,
    version: i64,
}

#[derive(Deserialize)]
pub struct SettingsForm {
    idle_timeout: String,
}

/// Shown when a save loses an optimistic-concurrency race (§11.4): the current text next to the
/// user's, with a form to save theirs over the newer version.
#[derive(Template)]
#[template(path = "conflict.html")]
struct ConflictPage {
    what: String,
    current_html: String,
    /// Who saved the newer version, e.g. "Claude · reviewer" or "You (in another tab or window)".
    current_editor: String,
    action: String,
    field: &'static str,
    yours: String,
    version: i64,
    cancel: String,
}

fn editor_label(editor: Option<Actor>) -> String {
    match editor {
        Some(Actor::Human) => "You (in another tab or window)".to_string(),
        Some(actor) => author(actor).label.to_string(),
        None => "Someone".to_string(),
    }
}

fn failed(error: Error, back: String, label: &str, submitted: &str) -> PageError {
    PageError::from(error)
        .back(back, label)
        .submitted(submitted)
}

fn redirect(url: &str) -> Response {
    Redirect::to(url).into_response()
}

pub async fn retitle(
    State(state): State<AppState>,
    Path(review_id): Path<i64>,
    Form(form): Form<TitleForm>,
) -> Result<Response, PageError> {
    let title = form.title.clone();
    state
        .call(move |service| service.retitle_review(Actor::Human, review_id, &title))
        .await
        .map_err(|error| {
            failed(
                error,
                review_url(review_id),
                "Back to the review",
                &form.title,
            )
        })?;
    Ok(redirect(&review_url(review_id)))
}

pub async fn change_base(
    State(state): State<AppState>,
    Path(review_id): Path<i64>,
    Form(form): Form<BaseForm>,
) -> Result<Response, PageError> {
    let base = if form.base.is_empty() {
        BaseMode::None
    } else {
        BaseMode::Ref(form.base.clone())
    };
    state
        .call(move |service| service.change_base(Actor::Human, review_id, &base))
        .await
        .map_err(|error| failed(error, review_url(review_id), "Back to the review", ""))?;
    Ok(redirect(&review_url(review_id)))
}

pub async fn edit_summary(
    State(state): State<AppState>,
    Path(review_id): Path<i64>,
    Form(form): Form<SummaryForm>,
) -> Result<Response, PageError> {
    let summary = form.summary.clone();
    let version = form.version;
    let result = state
        .call(move |service| service.update_summary(review_id, Actor::Human, &summary, version))
        .await;
    match result {
        Ok(_) => Ok(redirect(&format!("{}#summary", review_url(review_id)))),
        Err(Error::Conflict(_)) => {
            let current = state
                .call(move |service| service.get_review(review_id, None))
                .await?;
            let review = current.view.review;
            let page = ConflictPage {
                what: "the summary".to_string(),
                current_html: markdown::render(&review.summary),
                current_editor: editor_label(review.summary_updated_by),
                action: format!("{}/summary", review_url(review_id)),
                field: "summary",
                yours: form.summary,
                version: review.summary_version,
                cancel: format!("{}#summary", review_url(review_id)),
            };
            Ok((StatusCode::CONFLICT, Html(page.render()?)).into_response())
        }
        Err(error) => Err(failed(
            error,
            review_url(review_id),
            "Back to the review",
            &form.summary,
        )),
    }
}

pub async fn comment_on_summary(
    State(state): State<AppState>,
    Path(review_id): Path<i64>,
    Form(form): Form<BodyForm>,
) -> Result<Response, PageError> {
    let body = form.body.clone();
    let comment = state
        .call(move |service| {
            service.add_comment(Actor::Human, CommentTarget::Summary(review_id), &body)
        })
        .await
        .map_err(|error| {
            failed(
                error,
                review_url(review_id),
                "Back to the review",
                &form.body,
            )
        })?;
    Ok(redirect(&comment_url(&comment)))
}

pub async fn close_review(
    State(state): State<AppState>,
    Path(review_id): Path<i64>,
) -> Result<Response, PageError> {
    state
        .call(move |service| service.close_review(Actor::Human, review_id))
        .await
        .map_err(|error| failed(error, review_url(review_id), "Back to the review", ""))?;
    Ok(redirect(&review_url(review_id)))
}

pub async fn reopen_review(
    State(state): State<AppState>,
    Path(review_id): Path<i64>,
) -> Result<Response, PageError> {
    state
        .call(move |service| service.reopen_review(Actor::Human, review_id))
        .await
        .map_err(|error| failed(error, review_url(review_id), "Back to the review", ""))?;
    Ok(redirect(&review_url(review_id)))
}

/// Fail unless `issue_id` belongs to `review_id`, so URLs cannot mix reviews and issues.
fn require_issue_in_review(
    service: &crate::service::Service,
    review_id: i64,
    issue_id: i64,
) -> crate::error::Result<()> {
    if service.issue_record(issue_id)?.review_id == review_id {
        Ok(())
    } else {
        Err(Error::NotFound(format!(
            "issue {issue_id} is not part of review {review_id}"
        )))
    }
}

pub async fn comment_on_issue(
    State(state): State<AppState>,
    Path((review_id, issue_id)): Path<(i64, i64)>,
    Form(form): Form<BodyForm>,
) -> Result<Response, PageError> {
    let body = form.body.clone();
    let comment = state
        .call(move |service| {
            require_issue_in_review(service, review_id, issue_id)?;
            service.add_comment(Actor::Human, CommentTarget::Issue(issue_id), &body)
        })
        .await
        .map_err(|error| {
            failed(
                error,
                issue_url(review_id, issue_id),
                "Back to the issue",
                &form.body,
            )
        })?;
    Ok(redirect(&comment_url(&comment)))
}

pub async fn set_issue_status(
    State(state): State<AppState>,
    Path((review_id, issue_id)): Path<(i64, i64)>,
    Form(form): Form<StatusForm>,
) -> Result<Response, PageError> {
    let back = issue_url(review_id, issue_id);
    let status = match form.status.as_str() {
        "resolved" => IssueStatus::Resolved,
        "open" => IssueStatus::Open,
        other => {
            return Err(PageError::new(
                StatusCode::BAD_REQUEST,
                "Unknown status",
                &format!("{other:?} is not an issue status."),
            )
            .back(back, "Back to the issue"));
        }
    };
    let comment = (!form.comment.trim().is_empty()).then(|| form.comment.clone());
    state
        .call(move |service| {
            require_issue_in_review(service, review_id, issue_id)?;
            service.set_issue_status(Actor::Human, issue_id, status, comment.as_deref())
        })
        .await
        .map_err(|error| failed(error, back.clone(), "Back to the issue", &form.comment))?;
    Ok(redirect(&back))
}

pub async fn edit_comment(
    State(state): State<AppState>,
    Path(comment_id): Path<i64>,
    Form(form): Form<EditForm>,
) -> Result<Response, PageError> {
    let body = form.body.clone();
    let version = form.version;
    let result = state
        .call(move |service| service.edit_comment(Actor::Human, comment_id, &body, version))
        .await;
    match result {
        Ok(comment) => Ok(redirect(&comment_url(&comment))),
        Err(Error::Conflict(_)) => {
            let current = state
                .call(move |service| service.get_comment(comment_id))
                .await?;
            let page = ConflictPage {
                what: format!("comment c{comment_id}"),
                current_html: markdown::render(&current.body),
                current_editor: editor_label(Some(current.edited_by.unwrap_or(current.author))),
                action: format!("/comments/{comment_id}/edit"),
                field: "body",
                yours: form.body,
                version: current.version,
                cancel: comment_url(&current),
            };
            Ok((StatusCode::CONFLICT, Html(page.render()?)).into_response())
        }
        Err(error) => {
            let back = state
                .call(move |service| service.get_comment(comment_id))
                .await
                .map_or_else(|_| "/".to_string(), |comment| comment_url(&comment));
            Err(failed(error, back, "Back", &form.body))
        }
    }
}

pub async fn save_settings(
    State(state): State<AppState>,
    Form(form): Form<SettingsForm>,
) -> Result<Response, PageError> {
    let back = || "/settings".to_string();
    let timeout = IdleTimeout::parse(&form.idle_timeout)
        .map_err(|error| failed(error, back(), "Back to settings", &form.idle_timeout))?;
    state
        .call(move |service| service.set_idle_timeout(Actor::Human, timeout))
        .await
        .map_err(|error| failed(error, back(), "Back to settings", &form.idle_timeout))?;
    state.set_idle_timeout(timeout, TimeoutSource::Saved);
    Ok(redirect("/settings"))
}

/// `POST /api/shutdown`: used by `lani stop`.
pub async fn shutdown(State(state): State<AppState>) -> impl IntoResponse {
    state.request_shutdown();
    (StatusCode::OK, "stopping\n")
}
