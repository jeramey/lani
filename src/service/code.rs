//! What the web UI's code views show (docs/design.md §13.3): a directory or a file at the review's
//! head, each with the review's issues placed in the current code.

use std::collections::HashSet;

use super::{Placer, ReviewView, Service, is_in_file, repo_missing};
use crate::anchor;
use crate::error::{Error, Result};
use crate::git::{EntryKind, Git, TreeEntry};
use crate::model::{Issue, IssueStatus, short_ref, short_sha};
use crate::service::{IssueView, Location};
use crate::store;
use crate::text::quoted;

/// Files larger than this are not shown, only described.
pub const MAX_DISPLAY_BYTES: u64 = 10 << 20;

/// A path at the review's head, as a code view shows it.
#[derive(Debug)]
pub struct PathView {
    pub view: ReviewView,
    /// The head commit shown. Issues filed from the page anchor to it.
    pub head: String,
    /// The path shown, relative to the repository root; `""` for the root directory.
    pub path: String,
    pub content: PathContent,
}

#[derive(Debug)]
pub enum PathContent {
    Directory(Vec<DirectoryEntry>),
    File {
        file: FileContent,
        /// Whether the review's changes touch this file (diff reviews only).
        changed: bool,
        /// Every issue in this file, open or resolved, placed in the current code.
        issues: Vec<IssueView>,
    },
}

#[derive(Debug)]
pub struct DirectoryEntry {
    pub entry: TreeEntry,
    /// Whether the review's changes touch this file, or anything inside this directory.
    pub changed: bool,
    /// Open issues in this file, or anywhere inside this directory.
    pub open_issues: u64,
}

#[derive(Debug)]
pub enum FileContent {
    Text {
        /// The blob SHA, which identifies the content for caching.
        blob: String,
        /// Lines numbered as anchors number them ([`anchor::split_lines`]).
        lines: Vec<String>,
    },
    Binary {
        size: u64,
    },
    TooLarge {
        size: u64,
    },
    Symlink {
        target: String,
    },
    Submodule {
        commit: String,
    },
}

impl Service {
    /// The directory or file at `path` (`""` for the root) in the review's head commit.
    pub fn browse(&self, review_id: i64, path: &str) -> Result<PathView> {
        if !path.is_empty() {
            anchor::validate_path(path)?;
        }
        let view = self.review_view(store::get_review(&self.conn, review_id)?, true)?;
        let head = code_head(&view)?;
        let git = Git::new(&view.repo.git_dir);
        let changed = changed_paths(&git, &view)?;
        let entry = if path.is_empty() {
            None
        } else {
            let entry = git.tree_entry(&head, path)?.ok_or_else(|| {
                Error::NotFound(format!(
                    "{} does not exist at the head commit {}",
                    quoted(path),
                    short_sha(&head)
                ))
            })?;
            Some(entry)
        };
        let content = match entry {
            Some(entry) if entry.kind != EntryKind::Directory => PathContent::File {
                file: read_content(&git, &entry)?,
                changed: changed.contains(path),
                issues: self.issues_in_file(&view, path)?,
            },
            _ => {
                let open = self.open_issue_paths(&view)?;
                let entries = git
                    .list_directory(&head, path)?
                    .into_iter()
                    .map(|entry| directory_entry(entry, &changed, &open))
                    .collect();
                PathContent::Directory(entries)
            }
        };
        Ok(PathView {
            view,
            head,
            path: path.to_string(),
            content,
        })
    }

    /// Every issue in `path`, open or resolved, with its comments and code.
    fn issues_in_file(&self, view: &ReviewView, path: &str) -> Result<Vec<IssueView>> {
        let mut placer = Placer::new(view);
        let mut issues = Vec::new();
        for issue in store::list_issues(&self.conn, view.review.id)? {
            let location = placer.place(&issue.anchor);
            if is_in_file(&issue.anchor, &location, path) {
                issues.push(self.placed_issue_view(&mut placer, issue, location, true)?);
            }
        }
        Ok(issues)
    }

    /// The path each open issue is in now: its current path, or where it was opened when it
    /// is outdated.
    fn open_issue_paths(&self, view: &ReviewView) -> Result<Vec<String>> {
        let mut placer = Placer::new(view);
        Ok(store::list_issues(&self.conn, view.review.id)?
            .into_iter()
            .filter(|issue: &Issue| issue.status == IssueStatus::Open)
            .map(|issue| match placer.place(&issue.anchor) {
                Location::Current { path, .. } => path,
                Location::Outdated { .. } => issue.anchor.path,
            })
            .collect())
    }
}

/// The head commit a code view shows, or why there is none.
pub(super) fn code_head(view: &ReviewView) -> Result<String> {
    if view.repo_unavailable.is_some() {
        return Err(Error::Unavailable(repo_missing(&view.repo)));
    }
    view.git
        .as_ref()
        .and_then(|git| git.head_sha.clone())
        .ok_or_else(|| {
            Error::NotFound(format!(
                "head branch {} of review {} no longer exists, so there is no code to show",
                quoted(short_ref(&view.review.head_ref)),
                view.review.id
            ))
        })
}

/// Paths the review's changes touch, on either side of a rename; empty for a whole-tree review.
fn changed_paths(git: &Git, view: &ReviewView) -> Result<HashSet<String>> {
    let Some((merge_base, head)) = view
        .git
        .as_ref()
        .and_then(|state| Some((state.merge_base.as_ref()?, state.head_sha.as_ref()?)))
    else {
        return Ok(HashSet::new());
    };
    Ok(git
        .file_changes(merge_base, head)?
        .into_iter()
        .flat_map(|change| [change.old, change.new])
        .flatten()
        .map(|version| version.path)
        .collect())
}

fn directory_entry(entry: TreeEntry, changed: &HashSet<String>, open: &[String]) -> DirectoryEntry {
    let inside = |path: &str| {
        path == entry.path
            || (entry.kind == EntryKind::Directory
                && path
                    .strip_prefix(entry.path.as_str())
                    .is_some_and(|rest| rest.starts_with('/')))
    };
    DirectoryEntry {
        changed: changed.iter().any(|path| inside(path)),
        open_issues: open.iter().filter(|path| inside(path)).count() as u64,
        entry,
    }
}

fn read_content(git: &Git, entry: &TreeEntry) -> Result<FileContent> {
    let size = entry.size.unwrap_or(0);
    Ok(match entry.kind {
        EntryKind::Submodule => FileContent::Submodule {
            commit: entry.object.clone(),
        },
        EntryKind::Symlink => FileContent::Symlink {
            target: String::from_utf8_lossy(&git.read_blob(&entry.object)?).into_owned(),
        },
        _ if size > MAX_DISPLAY_BYTES => FileContent::TooLarge { size },
        _ => {
            let bytes = git.read_blob(&entry.object)?;
            if anchor::is_binary(&bytes) {
                FileContent::Binary { size }
            } else {
                FileContent::Text {
                    blob: entry.object.clone(),
                    lines: anchor::split_lines(&bytes),
                }
            }
        }
    })
}
