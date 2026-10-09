//! Form actions (POST). Each one redirects back to the page it came from (303 See Other), or
//! shows an error page that keeps the submitted text. Every action acts as the human.

use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::views::{PageError, author, comment_url, issue_url, review_url};
use super::{AppState, Stop, TimeoutSource};
use crate::base::BaseMode;
use crate::error::Error;
use crate::markdown;
use crate::model::{Actor, IssueStatus, Severity, Side};
use crate::service::{CommentTarget, CreateIssue, SpanRequest};
use crate::settings::IdleTimeout;

#[derive(Deserialize)]
pub struct TitleForm {
    title: String,
    #[serde(default)]
    back: Option<String>,
}

#[derive(Deserialize)]
pub struct BaseForm {
    /// A full ref name, or empty for a whole-tree review.
    base: String,
    #[serde(default)]
    back: Option<String>,
}

/// A form with nothing but the page to return to.
#[derive(Deserialize)]
pub struct BackForm {
    #[serde(default)]
    back: Option<String>,
}

#[derive(Deserialize)]
pub struct SummaryForm {
    summary: String,
    version: i64,
    #[serde(default)]
    back: Option<String>,
}

#[derive(Deserialize)]
pub struct BodyForm {
    body: String,
    /// The page to return to, when the form is not on the issue's own page.
    #[serde(default)]
    back: Option<String>,
}

#[derive(Deserialize)]
pub struct StatusForm {
    status: String,
    #[serde(default)]
    comment: String,
    #[serde(default)]
    back: Option<String>,
}

#[derive(Deserialize)]
pub struct EditForm {
    body: String,
    version: i64,
    #[serde(default)]
    back: Option<String>,
}

/// A new issue from a code view's line selection (§13.4).
#[derive(Deserialize)]
pub struct NewIssueForm {
    title: String,
    severity: String,
    path: String,
    side: String,
    start_line: u32,
    end_line: u32,
    /// The head commit the page was rendered at, which the issue anchors to.
    commit: String,
    /// For side "base": the merge-base the page showed.
    #[serde(default)]
    merge_base: Option<String>,
    body: String,
    #[serde(default)]
    back: Option<String>,
}

/// A new title and severity for an issue (§11.2: the human only).
#[derive(Deserialize)]
pub struct IssueDetailsForm {
    title: String,
    severity: String,
    version: i64,
}

/// The human handing a review on (§11.5).
#[derive(Deserialize)]
pub struct TurnForm {
    /// "reviewer", "implementer" or "nobody".
    to: String,
    #[serde(default)]
    note: String,
    #[serde(default)]
    back: Option<String>,
}

#[derive(Deserialize)]
pub struct RoundLimitForm {
    round_limit: String,
}

#[derive(Deserialize)]
pub struct PreviewForm {
    body: String,
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
    Ok(redirect(&overview(review_id, form.back.as_deref(), "")))
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
    Ok(redirect(&overview(review_id, form.back.as_deref(), "")))
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
        Ok(_) => Ok(redirect(&overview(
            review_id,
            form.back.as_deref(),
            "#summary",
        ))),
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
    Ok(redirect(&overview(
        review_id,
        form.back.as_deref(),
        &format!("#c{}", comment.id),
    )))
}

pub async fn close_review(
    State(state): State<AppState>,
    Path(review_id): Path<i64>,
    Form(form): Form<BackForm>,
) -> Result<Response, PageError> {
    state
        .call(move |service| service.close_review(Actor::Human, review_id))
        .await
        .map_err(|error| failed(error, review_url(review_id), "Back to the review", ""))?;
    Ok(redirect(&overview(review_id, form.back.as_deref(), "")))
}

pub async fn reopen_review(
    State(state): State<AppState>,
    Path(review_id): Path<i64>,
    Form(form): Form<BackForm>,
) -> Result<Response, PageError> {
    state
        .call(move |service| service.reopen_review(Actor::Human, review_id))
        .await
        .map_err(|error| failed(error, review_url(review_id), "Back to the review", ""))?;
    Ok(redirect(&overview(review_id, form.back.as_deref(), "")))
}

/// Where an overview form returns to: the overview as it was shown, with its issue filter and
/// activity page, which `lani.js` sends as `back` (§13.4), else the plain overview; then
/// `fragment`.
fn overview(review_id: i64, back: Option<&str>, fragment: &str) -> String {
    let page = back_page(back).unwrap_or_else(|| review_url(review_id));
    format!("{page}{fragment}")
}

/// The page a form asked to return to, without its fragment, if it is a local LANI path. Code
/// views send it so a reply or resolve made there lands back in the code.
fn back_page(back: Option<&str>) -> Option<String> {
    let page = back?.split('#').next()?;
    super::is_safe_next(page).then(|| page.to_string())
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
    let back = back_page(form.back.as_deref());
    let comment = state
        .call(move |service| {
            require_issue_in_review(service, review_id, issue_id)?;
            service.add_comment(Actor::Human, CommentTarget::Issue(issue_id), &body)
        })
        .await
        .map_err(|error| {
            failed(
                error,
                back.clone()
                    .unwrap_or_else(|| issue_url(review_id, issue_id)),
                "Back",
                &form.body,
            )
        })?;
    Ok(redirect(&match &back {
        Some(page) => format!("{page}#c{}", comment.id),
        None => comment_url(&comment),
    }))
}

/// `POST /reviews/{id}/issues`: open an issue on the lines selected in a code view. It anchors
/// to the commit the page was rendered at, so the line numbers mean what the page showed.
pub async fn create_issue(
    State(state): State<AppState>,
    Path(review_id): Path<i64>,
    Form(form): Form<NewIssueForm>,
) -> Result<Response, PageError> {
    let back = back_page(form.back.as_deref()).unwrap_or_else(|| review_url(review_id));
    let side = match form.side.as_str() {
        "head" => Side::Head,
        "base" => Side::Base,
        other => {
            return Err(PageError::new(
                StatusCode::BAD_REQUEST,
                "Unknown side",
                &format!("{other:?} is not a side of the diff."),
            )
            .back(back, "Back")
            .submitted(&form.body));
        }
    };
    let severity = parse_severity(&form.severity)
        .map_err(|error| error.back(back.clone(), "Back").submitted(&form.body))?;
    let (path, commit, body) = (form.path.clone(), form.commit.clone(), form.body.clone());
    let title = form.title.clone();
    let merge_base = form.merge_base.clone().filter(|sha| !sha.is_empty());
    let (start_line, end_line) = (form.start_line, form.end_line);
    let created = state
        .call(move |service| {
            service.create_issue(&CreateIssue {
                review_id,
                author: Actor::Human,
                title: &title,
                severity,
                body: &body,
                spans: vec![SpanRequest {
                    path: &path,
                    side,
                    start_line,
                    end_line,
                    start_line_text: None,
                }],
                commit: &commit,
                worktree_path: None,
                read_merge_base: merge_base.as_deref(),
                ask_human: false,
            })
        })
        .await
        .map_err(|error| failed(error, back.clone(), "Back", &form.body))?;
    Ok(redirect(&format!("{back}#issue-{}", created.issue.id)))
}

/// `POST /reviews/{id}/issues/{issue_id}/details`: change an issue's title and severity.
pub async fn edit_issue(
    State(state): State<AppState>,
    Path((review_id, issue_id)): Path<(i64, i64)>,
    Form(form): Form<IssueDetailsForm>,
) -> Result<Response, PageError> {
    let back = issue_url(review_id, issue_id);
    let severity = parse_severity(&form.severity).map_err(|error| {
        error
            .back(back.clone(), "Back to the issue")
            .submitted(&form.title)
    })?;
    let (title, version) = (form.title.clone(), form.version);
    state
        .call(move |service| {
            require_issue_in_review(service, review_id, issue_id)?;
            service.edit_issue(Actor::Human, issue_id, &title, severity, version)
        })
        .await
        .map_err(|error| failed(error, back.clone(), "Back to the issue", &form.title))?;
    Ok(redirect(&back))
}

fn parse_severity(value: &str) -> Result<Severity, PageError> {
    Severity::parse(value).ok_or_else(|| {
        PageError::new(
            StatusCode::BAD_REQUEST,
            "Choose a severity",
            &format!("{value:?} is not a severity; choose high, medium, low or nit."),
        )
    })
}

/// `POST /preview`: Markdown rendered as it will be shown, for the editors' Preview tab.
pub async fn preview(Form(form): Form<PreviewForm>) -> Result<Html<String>, PageError> {
    let html = tokio::task::spawn_blocking(move || markdown::render(&form.body))
        .await
        .map_err(|error| Error::Internal(format!("rendering the preview failed: {error}")))?;
    Ok(Html(html))
}

pub async fn set_issue_status(
    State(state): State<AppState>,
    Path((review_id, issue_id)): Path<(i64, i64)>,
    Form(form): Form<StatusForm>,
) -> Result<Response, PageError> {
    let return_to = back_page(form.back.as_deref());
    let back = return_to
        .clone()
        .unwrap_or_else(|| issue_url(review_id, issue_id));
    let status = match form.status.as_str() {
        "resolved" => IssueStatus::Resolved,
        "open" => IssueStatus::Open,
        other => {
            return Err(PageError::new(
                StatusCode::BAD_REQUEST,
                "Unknown status",
                &format!("{other:?} is not an issue status."),
            )
            .back(back, "Back"));
        }
    };
    let comment = (!form.comment.trim().is_empty()).then(|| form.comment.clone());
    state
        .call(move |service| {
            require_issue_in_review(service, review_id, issue_id)?;
            service.set_issue_status(Actor::Human, issue_id, status, comment.as_deref())
        })
        .await
        .map_err(|error| failed(error, back.clone(), "Back", &form.comment))?;
    Ok(redirect(&match return_to {
        Some(page) => format!("{page}#issue-{issue_id}"),
        None => back,
    }))
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
    let back = back_page(form.back.as_deref());
    match result {
        Ok(comment) => Ok(redirect(&match &back {
            Some(page) => format!("{page}#c{}", comment.id),
            None => comment_url(&comment),
        })),
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

/// `POST /reviews/{id}/turn`: hand the review to a Claude role, or to nobody.
pub async fn hand_off(
    State(state): State<AppState>,
    Path(review_id): Path<i64>,
    Form(form): Form<TurnForm>,
) -> Result<Response, PageError> {
    let back = review_url(review_id);
    let to = match form.to.as_str() {
        "reviewer" => Some(Actor::Reviewer),
        "implementer" => Some(Actor::Implementer),
        "nobody" => None,
        other => {
            return Err(PageError::new(
                StatusCode::BAD_REQUEST,
                "Unknown role",
                &format!("{other:?} is not someone a review can be handed to."),
            )
            .back(back, "Back to the review")
            .submitted(&form.note));
        }
    };
    let note = form.note.clone();
    state
        .call(move |service| {
            service.hand_off(Actor::Human, review_id, to, Some(&note))?;
            Ok(())
        })
        .await
        .map_err(|error| failed(error, back.clone(), "Back to the review", &form.note))?;
    Ok(redirect(&overview(review_id, form.back.as_deref(), "")))
}

/// `POST /settings/round-limit`: how many rounds Claude takes before the review comes to you.
pub async fn save_round_limit(
    State(state): State<AppState>,
    Form(form): Form<RoundLimitForm>,
) -> Result<Response, PageError> {
    let back = || "/settings".to_string();
    let limit = crate::settings::parse_round_limit(&form.round_limit)
        .map_err(|error| failed(error, back(), "Back to settings", &form.round_limit))?;
    state
        .call(move |service| service.set_round_limit(Actor::Human, limit))
        .await
        .map_err(|error| failed(error, back(), "Back to settings", &form.round_limit))?;
    Ok(redirect("/settings"))
}

/// `POST /api/shutdown`: used by `lani stop`.
pub async fn shutdown(State(state): State<AppState>) -> impl IntoResponse {
    state.request_shutdown(Stop::Requested);
    (StatusCode::OK, "stopping\n")
}
