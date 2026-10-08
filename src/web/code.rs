//! The code views (docs/design.md §13.3): the Files view of a directory or file at the review's
//! head, and the Changes view of the review's diff, with issue threads shown under their lines.

use std::collections::HashSet;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};

use super::AppState;
use super::views::{self, Crumb, MarkerView, PageError, ReviewHeader, ThreadView};
use serde::Deserialize;

use crate::git::{DiffHunk, EntryKind, RowKind};
use crate::highlight::{self, HighlightedLines};
use crate::model::{Anchor, IssueStatus, ReviewStatus, Side, short_sha};
use crate::service::{
    ChangedFile, FileContent, FileDiff, FileText, Location, PathContent, PathView,
};

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

/// One line of a file, with the threads of issues whose first span here ends on it, and markers
/// for issues whose later spans end on it.
struct FileLine {
    number: u32,
    html: String,
    threads: Vec<ThreadView>,
    markers: Vec<MarkerView>,
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
    /// Where the new-issue form posts.
    create_action: String,
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
                    markers: Vec::new(),
                })
                .collect()
        })
        .unwrap_or_default();
    let has_diff = view.view.review.base_ref.is_some();
    let mut other_threads = Vec::new();
    for issue in &issues {
        // The last lines of this issue's current spans in this file, in file order. The thread
        // goes under the first; the others get a marker pointing to it.
        let mut ends: Vec<usize> = issue
            .spans()
            .filter_map(|(anchor, location)| inline_line(anchor, location, &view.head, &view.path))
            .filter_map(|end_line| usize::try_from(end_line).ok())
            .filter(|&end_line| end_line <= lines.len())
            .collect();
        ends.sort_unstable();
        ends.dedup();
        match ends.split_first() {
            Some((&first, rest)) => {
                lines[first - 1]
                    .threads
                    .push(views::thread(issue, &back, false, has_diff));
                for &end_line in rest {
                    lines[end_line - 1].markers.push(views::marker(issue));
                }
            }
            None => other_threads.push(views::thread(issue, &back, true, has_diff)),
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
        create_action: format!("{}/issues", views::review_url(review_id)),
        review,
    }
    .render()?)
}

/// The line a span ends on in the file view: the end of its current lines, when it is on the
/// head side of this file.
fn inline_line(anchor: &Anchor, location: &Location, head: &str, path: &str) -> Option<u32> {
    match location {
        Location::Current {
            commit,
            path: current,
            end_line,
            ..
        } if anchor.side == Side::Head && commit == head && current == path => Some(*end_line),
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

// ── changes ──────────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ChangesQuery {
    /// A file whose collapsed diff should be loaded.
    expand: Option<String>,
}

/// One row of a file's diff.
struct DiffRowView {
    /// "context", "added" or "removed", for styling.
    class: &'static str,
    mark: &'static str,
    old: Option<u32>,
    new: Option<u32>,
    /// The side a selection of this row anchors to, and its line number there: "base" for
    /// removed lines, "head" otherwise (§13.4).
    side: &'static str,
    line: u32,
    /// The row's element id, e.g. "file-…-H42".
    id: String,
    html: String,
    threads: Vec<ThreadView>,
    markers: Vec<MarkerView>,
}

struct HunkView {
    header: String,
    rows: Vec<DiffRowView>,
}

struct FileSection {
    anchor: String,
    status_label: &'static str,
    status_class: &'static str,
    /// The path, or "old → new" for a rename.
    title: String,
    /// The path issues on each side anchor to.
    head_path: String,
    base_path: String,
    added: u32,
    removed: u32,
    open_issues: usize,
    /// The file in the Files view, when it exists at the head.
    file_url: Option<String>,
    notice: Option<String>,
    expand_url: Option<String>,
    hunks: Vec<HunkView>,
    other_threads: Vec<ThreadView>,
}

#[derive(Template)]
#[template(path = "changes.html")]
struct ChangesPage {
    review: ReviewHeader,
    editable: bool,
    head: String,
    merge_base: String,
    head_short: String,
    merge_base_short: String,
    totals: String,
    files: Vec<FileSection>,
    /// Where the new-issue form posts.
    create_action: String,
}

/// `GET /reviews/{id}/changes`: the unified diff from the merge-base to the head (§13.3).
pub async fn changes(
    State(state): State<AppState>,
    Path(review_id): Path<i64>,
    Query(query): Query<ChangesQuery>,
) -> Result<Response, PageError> {
    let expand = query.expand.clone();
    let (view, highlighted) = state
        .call(move |service| {
            let view = service.changes(review_id, expand.as_deref())?;
            let highlighted: Vec<_> = view
                .files
                .iter()
                .map(|file| match &file.diff {
                    FileDiff::Text { old, new, .. } => {
                        let highlight = |text: &Option<FileText>, path: Option<&str>| {
                            text.as_ref().map(|text| {
                                highlight::highlight_file(
                                    &text.blob,
                                    file_name(path.unwrap_or("")),
                                    &text.lines,
                                )
                            })
                        };
                        (
                            highlight(old, file.change.old.as_ref().map(|v| v.path.as_str())),
                            highlight(new, file.change.new.as_ref().map(|v| v.path.as_str())),
                        )
                    }
                    _ => (None, None),
                })
                .collect();
            Ok((view, highlighted))
        })
        .await
        .map_err(|error| {
            PageError::from(error).back(views::review_url(review_id), "Back to the review")
        })?;
    let back = match &query.expand {
        Some(path) => format!(
            "{}?expand={}",
            views::changes_url(review_id),
            views::encode_path(path).replace('/', "%2F")
        ),
        None => views::changes_url(review_id),
    };
    let (mut total_added, mut total_removed) = (0_u64, 0_u64);
    let mut files = Vec::new();
    // An issue with spans in several files gets one thread on the page; the files after the
    // first show markers.
    let mut shown = HashSet::new();
    for (file, (old, new)) in view.files.iter().zip(&highlighted) {
        total_added += u64::from(file.added);
        total_removed += u64::from(file.removed);
        files.push(file_section(
            review_id,
            (&view.head, &view.merge_base),
            file,
            (old.as_deref(), new.as_deref()),
            &back,
            &mut shown,
        ));
    }
    let mut header = views::review_header(&view.view);
    header.tab = "changes";
    let page = ChangesPage {
        editable: view.view.review.status == ReviewStatus::Open,
        head_short: short_sha(&view.head).to_string(),
        merge_base_short: short_sha(&view.merge_base).to_string(),
        totals: format!(
            "{} · +{total_added} −{total_removed}",
            plural(files.len(), "file")
        ),
        head: view.head,
        merge_base: view.merge_base,
        files,
        create_action: format!("{}/issues", views::review_url(review_id)),
        review: header,
    };
    Ok(Html(page.render()?).into_response())
}

fn file_section(
    review_id: i64,
    (head, merge_base): (&str, &str),
    file: &ChangedFile,
    (old, new): (Option<&HighlightedLines>, Option<&HighlightedLines>),
    back: &str,
    shown: &mut HashSet<i64>,
) -> FileSection {
    let change = &file.change;
    let old_path = change.old.as_ref().map(|version| version.path.as_str());
    let new_path = change.new.as_ref().map(|version| version.path.as_str());
    let path = change.path();
    let anchor = views::file_anchor(path);
    let (status_label, status_class) = match change.status {
        'A' => ("Added", "added"),
        'D' => ("Deleted", "deleted"),
        'R' => ("Renamed", "renamed"),
        'T' => ("Type changed", "modified"),
        _ => ("Modified", "modified"),
    };
    let title = match (old_path, new_path) {
        (Some(old), Some(new)) if old != new => format!("{old} → {new}"),
        _ => path.to_string(),
    };
    let mut hunks: Vec<HunkView> = match &file.diff {
        FileDiff::Text { hunks, .. } => hunks
            .iter()
            .map(|hunk| hunk_view(hunk, &anchor, old, new))
            .collect(),
        _ => Vec::new(),
    };
    let (notice, expand_url) = diff_notice(review_id, file, &anchor);
    let other_threads = place_threads(
        file,
        &mut hunks,
        (head, new_path),
        (merge_base, old_path),
        back,
        shown,
    );
    FileSection {
        status_label,
        status_class,
        title,
        head_path: new_path.unwrap_or("").to_string(),
        base_path: old_path.unwrap_or("").to_string(),
        added: file.added,
        removed: file.removed,
        open_issues: file
            .issues
            .iter()
            .filter(|issue| issue.issue.status == IssueStatus::Open)
            .count(),
        file_url: new_path.map(|path| views::files_url(review_id, path, false)),
        notice,
        expand_url,
        hunks,
        other_threads,
        anchor,
    }
}

/// Why a file's diff is not shown, and for a collapsed diff the link that loads it.
fn diff_notice(
    review_id: i64,
    file: &ChangedFile,
    anchor: &str,
) -> (Option<String>, Option<String>) {
    let notice = match &file.diff {
        FileDiff::Text { .. } => return (None, None),
        FileDiff::Collapsed => {
            let notice = format!(
                "This diff changes {} lines, more than {} are shown at once.",
                file.added + file.removed,
                crate::service::MAX_EXPANDED_CHANGES
            );
            let expand_url = format!(
                "{}?expand={}#{anchor}",
                views::changes_url(review_id),
                views::encode_path(file.change.path()).replace('/', "%2F")
            );
            return (Some(notice), Some(expand_url));
        }
        FileDiff::Binary => "Binary file, not shown.".to_string(),
        FileDiff::TooLarge => format!(
            "This file is larger than {} MiB, so its diff is not shown.",
            crate::service::MAX_DISPLAY_BYTES >> 20
        ),
        FileDiff::Submodule { old, new } => {
            let commit = |sha: &Option<String>| {
                sha.as_deref()
                    .map_or("none".to_string(), |sha| short_sha(sha).to_string())
            };
            format!("Submodule changed from {} to {}.", commit(old), commit(new))
        }
        FileDiff::Unchanged => match file.change.status {
            'R' => "Renamed without changes to its content.",
            'A' => "Empty file added.",
            'D' => "Empty file deleted.",
            _ => "Only the file mode changed.",
        }
        .to_string(),
    };
    (Some(notice), None)
}

/// Put each issue's thread under the diff row its first span on the page ends on, and a marker
/// on the rows its other spans end on. An issue whose thread is already on the page (`shown`)
/// gets only markers. Returns the threads that are on no row: outdated, or outside the hunks.
fn place_threads(
    file: &ChangedFile,
    hunks: &mut [HunkView],
    (head, new_path): (&str, Option<&str>),
    (merge_base, old_path): (&str, Option<&str>),
    back: &str,
    shown: &mut HashSet<i64>,
) -> Vec<ThreadView> {
    let mut other_threads = Vec::new();
    for issue in &file.issues {
        let mut rows: Vec<(usize, usize)> = issue
            .spans()
            .filter_map(|(anchor, location)| match (location, anchor.side) {
                (
                    Location::Current {
                        commit,
                        path,
                        end_line,
                        ..
                    },
                    Side::Head,
                ) if commit == head && Some(path.as_str()) == new_path => {
                    row_position(hunks, |row| row.new == Some(*end_line))
                }
                (
                    Location::Current {
                        commit,
                        path,
                        end_line,
                        ..
                    },
                    Side::Base,
                ) if commit == merge_base && Some(path.as_str()) == old_path => {
                    row_position(hunks, |row| row.old == Some(*end_line))
                }
                _ => None,
            })
            .collect();
        rows.sort_unstable();
        rows.dedup();
        let first_on_page = shown.insert(issue.issue.id);
        let markers = match (first_on_page, rows.split_first()) {
            (true, Some((&(hunk, row), rest))) => {
                hunks[hunk].rows[row]
                    .threads
                    .push(views::thread(issue, back, false, true));
                rest
            }
            (true, None) => {
                other_threads.push(views::thread(issue, back, true, true));
                &[][..]
            }
            (false, _) => &rows[..],
        };
        for &(hunk, row) in markers {
            hunks[hunk].rows[row].markers.push(views::marker(issue));
        }
    }
    other_threads
}

/// The position (hunk, row) of the first row that matches.
fn row_position(
    hunks: &[HunkView],
    matches: impl Fn(&DiffRowView) -> bool,
) -> Option<(usize, usize)> {
    hunks.iter().enumerate().find_map(|(hunk_index, hunk)| {
        hunk.rows
            .iter()
            .position(&matches)
            .map(|row_index| (hunk_index, row_index))
    })
}

fn hunk_view(
    hunk: &DiffHunk,
    anchor: &str,
    old: Option<&HighlightedLines>,
    new: Option<&HighlightedLines>,
) -> HunkView {
    let line_html = |lines: Option<&HighlightedLines>, number: Option<u32>| {
        number
            .and_then(|number| {
                lines?
                    .lines
                    .get(usize::try_from(number).ok()?.checked_sub(1)?)
            })
            .cloned()
            .unwrap_or_default()
    };
    let header = &hunk.header;
    HunkView {
        header: format!(
            "@@ -{},{} +{},{} @@",
            header.old_start, header.old_count, header.new_start, header.new_count
        ),
        rows: hunk
            .rows
            .iter()
            .map(|row| {
                let (class, mark, side, line, html) = match row.kind {
                    RowKind::Removed => {
                        let line = row.old_line.unwrap_or(0);
                        ("removed", "-", "base", line, line_html(old, row.old_line))
                    }
                    RowKind::Added => {
                        let line = row.new_line.unwrap_or(0);
                        ("added", "+", "head", line, line_html(new, row.new_line))
                    }
                    RowKind::Context => {
                        let line = row.new_line.unwrap_or(0);
                        ("context", " ", "head", line, line_html(new, row.new_line))
                    }
                };
                let side_letter = if side == "base" { 'B' } else { 'H' };
                DiffRowView {
                    class,
                    mark,
                    old: row.old_line,
                    new: row.new_line,
                    side,
                    line,
                    id: format!("{anchor}-{side_letter}{line}"),
                    html,
                    threads: Vec::new(),
                    markers: Vec::new(),
                }
            })
            .collect(),
    }
}
