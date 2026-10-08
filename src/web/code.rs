//! The code views (docs/design.md §13.3): the Files view of a directory or file at the review's
//! head, with issue threads shown under their lines.

use askama::Template;
use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse, Redirect, Response};

use super::AppState;
use super::views::{self, Crumb, PageError, ReviewHeader, ThreadView};
use crate::git::EntryKind;
use crate::highlight::{self, HighlightedLines};
use crate::model::{ReviewStatus, Side, short_sha};
use crate::service::{FileContent, IssueView, Location, PathContent, PathView};

struct EntryRow {
    name: String,
    url: String,
    /// "dir", "file", "link" or "submodule", for styling.
    kind: &'static str,
    changed: bool,
    open_issues: u64,
}

#[derive(Template)]
#[template(path = "tree.html")]
struct TreePage {
    review: ReviewHeader,
    crumbs: Vec<Crumb>,
    head_short: String,
    parent_url: Option<String>,
    entries: Vec<EntryRow>,
}

/// One line of a file, with the threads of issues that end on it.
struct FileLine {
    number: u32,
    html: String,
    threads: Vec<ThreadView>,
}

#[derive(Template)]
#[template(path = "file.html")]
struct FilePage {
    review: ReviewHeader,
    editable: bool,
    crumbs: Vec<Crumb>,
    path: String,
    /// The full head SHA, which issues filed from this page anchor to.
    head: String,
    head_short: String,
    /// E.g. "Rust · 120 lines".
    facts: String,
    changes_url: Option<String>,
    /// Why there are no lines, or why they are not highlighted.
    notice: Option<String>,
    lines: Vec<FileLine>,
    /// Issues that cannot be shown under a current line of this file.
    other_threads: Vec<ThreadView>,
}

/// `GET /reviews/{id}/files`: the root directory lives at `/files/`.
pub async fn files_bare(Path(review_id): Path<i64>) -> Redirect {
    Redirect::to(&views::files_url(review_id, "", true))
}

/// `GET /reviews/{id}/files/`: the root directory at the head.
pub async fn files_root(
    State(state): State<AppState>,
    Path(review_id): Path<i64>,
) -> Result<Response, PageError> {
    browse(&state, review_id, String::new(), true).await
}

/// `GET /reviews/{id}/files/{path}`: a file, or a directory when the path ends with `/`.
pub async fn files(
    State(state): State<AppState>,
    Path((review_id, path)): Path<(i64, String)>,
) -> Result<Response, PageError> {
    let directory_url = path.ends_with('/');
    let path = path.trim_end_matches('/').to_string();
    browse(&state, review_id, path, directory_url).await
}

async fn browse(
    state: &AppState,
    review_id: i64,
    path: String,
    directory_url: bool,
) -> Result<Response, PageError> {
    let (view, highlighted) = state
        .call(move |service| {
            let view = service.browse(review_id, &path)?;
            let highlighted = match &view.content {
                PathContent::File {
                    file: FileContent::Text { blob, lines },
                    ..
                } => Some(highlight::highlight_file(
                    blob,
                    file_name(&view.path),
                    lines,
                )),
                _ => None,
            };
            Ok((view, highlighted))
        })
        .await
        .map_err(|error| {
            PageError::from(error).back(views::files_url(review_id, "", true), "All files")
        })?;
    let is_directory = matches!(view.content, PathContent::Directory(_));
    // Directory URLs end with "/" and file URLs don't, so relative links behave and every
    // path has one address.
    if !view.path.is_empty() && is_directory != directory_url {
        return Ok(
            Redirect::to(&views::files_url(review_id, &view.path, is_directory)).into_response(),
        );
    }
    let mut header = views::review_header(&view.view);
    header.tab = "files";
    let page = if is_directory {
        tree_page(header, &view)?
    } else {
        file_page(header, view, highlighted.as_deref())?
    };
    Ok(Html(page).into_response())
}

fn tree_page(review: ReviewHeader, view: &PathView) -> Result<String, PageError> {
    let PathContent::Directory(entries) = &view.content else {
        return Ok(String::new());
    };
    let review_id = view.view.review.id;
    let mut rows: Vec<EntryRow> = entries
        .iter()
        .map(|item| {
            let entry = &item.entry;
            let is_dir = entry.kind == EntryKind::Directory;
            EntryRow {
                name: entry.name().to_string(),
                url: views::files_url(review_id, &entry.path, is_dir),
                kind: match entry.kind {
                    EntryKind::Directory => "dir",
                    EntryKind::File => "file",
                    EntryKind::Symlink => "link",
                    EntryKind::Submodule => "submodule",
                },
                changed: item.changed,
                open_issues: item.open_issues,
            }
        })
        .collect();
    // Directories first, like most file browsers; git already sorts by name.
    rows.sort_by_key(|row| row.kind != "dir");
    let parent_url = (!view.path.is_empty()).then(|| {
        let parent = view.path.rsplit_once('/').map_or("", |(parent, _)| parent);
        views::files_url(review_id, parent, true)
    });
    Ok(TreePage {
        crumbs: views::crumbs(review_id, &view.view.repo.name, &view.path),
        head_short: short_sha(&view.head).to_string(),
        parent_url,
        entries: rows,
        review,
    }
    .render()?)
}

fn file_page(
    review: ReviewHeader,
    view: PathView,
    highlighted: Option<&HighlightedLines>,
) -> Result<String, PageError> {
    let PathContent::File {
        file,
        changed,
        issues,
    } = view.content
    else {
        return Ok(String::new());
    };
    let review_id = view.view.review.id;
    let back = views::files_url(review_id, &view.path, false);
    let (notice, facts) = match (&file, highlighted) {
        (FileContent::Text { lines, .. }, Some(highlighted)) => (
            highlighted
                .plain_reason
                .map(|reason| format!("This file is {reason}.")),
            format!("{} · {}", highlighted.language, plural(lines.len(), "line")),
        ),
        (FileContent::Text { lines, .. }, None) => (None, plural(lines.len(), "line")),
        (FileContent::Binary { size }, _) => (
            Some("This is a binary file, so it is not shown.".to_string()),
            plural_bytes(*size),
        ),
        (FileContent::TooLarge { size }, _) => (
            Some(format!(
                "This file is larger than {} MiB, so it is not shown.",
                crate::service::MAX_DISPLAY_BYTES >> 20
            )),
            plural_bytes(*size),
        ),
        (FileContent::Symlink { target }, _) => (
            Some(format!("This is a symbolic link to {target}.")),
            "symbolic link".to_string(),
        ),
        (FileContent::Submodule { commit }, _) => (
            Some(format!(
                "This is a submodule at commit {}.",
                short_sha(commit)
            )),
            "submodule".to_string(),
        ),
    };
    let mut lines: Vec<FileLine> = highlighted
        .map(|highlighted| {
            (1..)
                .zip(&highlighted.lines)
                .map(|(number, html)| FileLine {
                    number,
                    html: html.clone(),
                    threads: Vec::new(),
                })
                .collect()
        })
        .unwrap_or_default();
    let mut other_threads = Vec::new();
    for issue in &issues {
        match inline_line(issue, &view.head, &view.path) {
            Some(end_line) if (end_line as usize) <= lines.len() => {
                lines[end_line as usize - 1]
                    .threads
                    .push(views::thread(issue, &back, false));
            }
            _ => other_threads.push(views::thread(issue, &back, true)),
        }
    }
    Ok(FilePage {
        editable: view.view.review.status == ReviewStatus::Open,
        crumbs: views::crumbs(review_id, &view.view.repo.name, &view.path),
        head_short: short_sha(&view.head).to_string(),
        changes_url: changed.then(|| {
            format!(
                "{}#{}",
                views::changes_url(review_id),
                views::file_anchor(&view.path)
            )
        }),
        head: view.head,
        path: view.path,
        facts,
        notice,
        lines,
        other_threads,
        review,
    }
    .render()?)
}

/// The line under which an issue's thread belongs in the file view: the end of its current
/// lines, when it is on the head side of this file.
fn inline_line(issue: &IssueView, head: &str, path: &str) -> Option<u32> {
    match &issue.location {
        Location::Current {
            commit,
            path: current,
            end_line,
            ..
        } if issue.issue.anchor.side == Side::Head && commit == head && current == path => {
            Some(*end_line)
        }
        _ => None,
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

fn plural_bytes(size: u64) -> String {
    plural(usize::try_from(size).unwrap_or(usize::MAX), "byte")
}
