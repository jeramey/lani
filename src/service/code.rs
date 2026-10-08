//! What the web UI's code views show (docs/design.md §13.3): a directory or a file at the review's
//! head, each with the review's issues placed in the current code.

use std::collections::{HashMap, HashSet};

use super::{Placer, ReviewView, Service, is_in_file, repo_missing};
use crate::anchor;
use crate::error::{Error, Result};
use crate::git::{DiffHunk, EntryKind, FileChange, FileVersion, Git, RowKind, TreeEntry};
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
    /// The review's file changes, for links to the Changes view ([`review_changes`]).
    pub changes: Vec<FileChange>,
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
        let changes = review_changes(&view)?;
        // Paths the review's changes touch, on either side of a rename.
        let changed_paths: HashSet<&str> = changes
            .iter()
            .flat_map(|change| [&change.old, &change.new])
            .flatten()
            .map(|version| version.path.as_str())
            .collect();
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
                changed: changed_paths.contains(path),
                issues: self.issues_in_file(&view, path)?,
            },
            _ => {
                let open = self.open_issue_paths(&view)?;
                let entries = git
                    .list_directory(&head, path)?
                    .into_iter()
                    .map(|entry| directory_entry(entry, &changed_paths, &open))
                    .collect();
                PathContent::Directory(entries)
            }
        };
        Ok(PathView {
            view,
            head,
            path: path.to_string(),
            content,
            changes,
        })
    }

    /// Every issue in `path`, open or resolved, with its comments and code.
    fn issues_in_file(&self, view: &ReviewView, path: &str) -> Result<Vec<IssueView>> {
        let mut placer = Placer::new(view);
        let mut issues = Vec::new();
        for issue in store::list_issues(&self.conn, view.review.id)? {
            let locations = placer.place_all(&issue.anchors);
            let in_file = issue
                .anchors
                .iter()
                .zip(&locations)
                .any(|(anchor, location)| is_in_file(anchor, location, path));
            if in_file {
                issues.push(self.placed_issue_view(&mut placer, issue, locations, true)?);
            }
        }
        Ok(issues)
    }

    /// For each open issue, the paths its spans are in now: each span's current path, or where
    /// it was opened when it is outdated.
    fn open_issue_paths(&self, view: &ReviewView) -> Result<Vec<Vec<String>>> {
        let mut placer = Placer::new(view);
        Ok(store::list_issues(&self.conn, view.review.id)?
            .into_iter()
            .filter(|issue: &Issue| issue.status == IssueStatus::Open)
            .map(|issue| {
                let locations = placer.place_all(&issue.anchors);
                issue
                    .anchors
                    .into_iter()
                    .zip(locations)
                    .map(|(anchor, location)| match location {
                        Location::Current { path, .. } => path,
                        Location::Outdated { .. } => anchor.path,
                    })
                    .collect()
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

/// The files the review's changes touch, from the merge-base to the head, in the Changes view's
/// order. Empty for a whole-tree review, and when the repository, the head or the merge-base is
/// missing (`view` must have been loaded with git state).
pub fn review_changes(view: &ReviewView) -> Result<Vec<FileChange>> {
    let Some((merge_base, head)) = view
        .git
        .as_ref()
        .and_then(|state| Some((state.merge_base.as_ref()?, state.head_sha.as_ref()?)))
    else {
        return Ok(Vec::new());
    };
    Git::new(&view.repo.git_dir).file_changes(merge_base, head)
}

/// A directory entry with its marks. `open` holds each open issue's paths; an issue counts once
/// for an entry however many of its spans are inside it.
fn directory_entry(
    entry: TreeEntry,
    changed: &HashSet<&str>,
    open: &[Vec<String>],
) -> DirectoryEntry {
    let inside = |path: &str| {
        path == entry.path
            || (entry.kind == EntryKind::Directory
                && path
                    .strip_prefix(entry.path.as_str())
                    .is_some_and(|rest| rest.starts_with('/')))
    };
    DirectoryEntry {
        changed: changed.iter().any(|path| inside(path)),
        open_issues: open
            .iter()
            .filter(|paths| paths.iter().any(|path| inside(path)))
            .count() as u64,
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

/// Diffs with more changed lines than this are collapsed until asked for.
pub const MAX_EXPANDED_CHANGES: u32 = 1_500;

/// The review's changes, from the merge-base to the head, as the Changes view shows them.
#[derive(Debug)]
pub struct ChangesView {
    pub view: ReviewView,
    pub head: String,
    pub merge_base: String,
    pub files: Vec<ChangedFile>,
}

#[derive(Debug)]
pub struct ChangedFile {
    pub change: FileChange,
    pub added: u32,
    pub removed: u32,
    pub diff: FileDiff,
    /// Every issue with a span on either path of this file, open or resolved, placed in the
    /// current code. An issue with spans in several files is listed with each of them.
    pub issues: Vec<IssueView>,
}

#[derive(Debug)]
pub enum FileDiff {
    Text {
        hunks: Vec<DiffHunk>,
        /// The lines before and after the change, for the sides that exist.
        old: Option<FileText>,
        new: Option<FileText>,
    },
    /// More than [`MAX_EXPANDED_CHANGES`] changed lines, not loaded unless asked for.
    Collapsed,
    Binary,
    TooLarge,
    Submodule {
        old: Option<String>,
        new: Option<String>,
    },
    /// A rename or mode change that leaves the content as it was.
    Unchanged,
}

/// A text file's content at one commit.
#[derive(Debug)]
pub struct FileText {
    pub blob: String,
    pub lines: Vec<String>,
}

impl Service {
    /// The review's changes from the merge-base to the head. Collapsed diffs are loaded only for
    /// the file named by `expand`.
    pub fn changes(&self, review_id: i64, expand: Option<&str>) -> Result<ChangesView> {
        let view = self.review_view(store::get_review(&self.conn, review_id)?, true)?;
        let head = code_head(&view)?;
        if view.review.base_ref.is_none() {
            return Err(Error::NotFound(format!(
                "review {review_id} is a whole-tree review, so it has no diff; see its files instead"
            )));
        }
        let merge_base = view
            .git
            .as_ref()
            .and_then(|git| git.merge_base.clone())
            .ok_or_else(|| {
                Error::NotFound(format!(
                    "review {review_id}'s base cannot be resolved, so there is no diff to show"
                ))
            })?;
        let git = Git::new(&view.repo.git_dir);
        let mut placer = Placer::new(&view);
        let all_issues: Vec<(Issue, Vec<Location>)> = store::list_issues(&self.conn, review_id)?
            .into_iter()
            .map(|issue| {
                let locations = placer.place_all(&issue.anchors);
                (issue, locations)
            })
            .collect();
        let mut files = Vec::new();
        for (change, added, removed, diff) in load_diffs(&git, &merge_base, &head, expand)? {
            let paths: Vec<&str> = [&change.old, &change.new]
                .into_iter()
                .flatten()
                .map(|version| version.path.as_str())
                .collect();
            // An issue with spans in several files belongs to each of them; the page decides
            // where its thread goes.
            let mut issues = Vec::new();
            for (issue, locations) in &all_issues {
                let belongs = issue
                    .anchors
                    .iter()
                    .zip(locations)
                    .any(|(anchor, location)| {
                        paths.iter().any(|path| is_in_file(anchor, location, path))
                    });
                if belongs {
                    issues.push(self.placed_issue_view(
                        &mut placer,
                        issue.clone(),
                        locations.clone(),
                        true,
                    )?);
                }
            }
            files.push(ChangedFile {
                change,
                added,
                removed,
                diff,
                issues,
            });
        }
        Ok(ChangesView {
            view,
            head,
            merge_base,
            files,
        })
    }
}

/// Every changed file between `merge_base` and `head` with its added and removed line counts and
/// its diff. This runs one diff per file; then all blob sizes, and all blobs to show, are read
/// in one git call each.
fn load_diffs(
    git: &Git,
    merge_base: &str,
    head: &str,
    expand: Option<&str>,
) -> Result<Vec<(FileChange, u32, u32, FileDiff)>> {
    let mut diffs = Vec::new();
    for change in git.file_changes(merge_base, head)? {
        let hunks = git.diff_file(
            merge_base,
            head,
            change.old.as_ref().map(|version| version.path.as_str()),
            change.new.as_ref().map(|version| version.path.as_str()),
        )?;
        let (added, removed) = count_changes(&hunks);
        diffs.push((change, hunks, added, removed));
    }
    let text_objects: Vec<&str> = diffs
        .iter()
        .filter(|(change, hunks, ..)| !hunks.is_empty() && !is_submodule(change))
        .flat_map(|(change, ..)| [&change.old, &change.new])
        .flatten()
        .map(|version| version.object.as_str())
        .collect();
    let sizes = git.object_sizes(&text_objects)?;
    let plans: Vec<Plan> = diffs
        .iter()
        .map(|(change, hunks, added, removed)| {
            let expanded = expand.is_some_and(|path| {
                [&change.old, &change.new]
                    .into_iter()
                    .flatten()
                    .any(|version| version.path == path)
            });
            plan(change, hunks, added + removed, expanded, &sizes)
        })
        .collect();
    let to_read: Vec<&str> = diffs
        .iter()
        .zip(&plans)
        .filter(|(_, plan)| matches!(plan, Plan::Read))
        .flat_map(|((change, ..), _)| [&change.old, &change.new])
        .flatten()
        .map(|version| version.object.as_str())
        .collect();
    let blobs = git.read_blobs(&to_read)?;
    diffs
        .into_iter()
        .zip(plans)
        .map(|((change, hunks, added, removed), plan)| {
            let diff = match plan {
                Plan::Done(diff) => diff,
                Plan::Read => text_diff(&change, hunks, &blobs)?,
            };
            Ok((change, added, removed, diff))
        })
        .collect()
}

fn count_changes(hunks: &[DiffHunk]) -> (u32, u32) {
    let rows = hunks.iter().flat_map(|hunk| &hunk.rows);
    let count = |kind| u32::try_from(rows.clone().filter(|row| row.kind == kind).count());
    (
        count(RowKind::Added).unwrap_or(u32::MAX),
        count(RowKind::Removed).unwrap_or(u32::MAX),
    )
}

/// What to do with a changed file once its diff and blob sizes are known.
enum Plan {
    Done(FileDiff),
    /// Read both blobs and show the diff.
    Read,
}

fn is_submodule(change: &FileChange) -> bool {
    [&change.old, &change.new]
        .into_iter()
        .flatten()
        .any(|version| version.mode == "160000")
}

fn plan(
    change: &FileChange,
    hunks: &[DiffHunk],
    changed_lines: u32,
    expanded: bool,
    sizes: &HashMap<String, u64>,
) -> Plan {
    if is_submodule(change) {
        let commit = |version: &Option<FileVersion>| {
            version
                .as_ref()
                .filter(|version| version.mode == "160000")
                .map(|version| version.object.clone())
        };
        return Plan::Done(FileDiff::Submodule {
            old: commit(&change.old),
            new: commit(&change.new),
        });
    }
    if hunks.is_empty() {
        return Plan::Done(FileDiff::Unchanged);
    }
    let too_large = [&change.old, &change.new]
        .into_iter()
        .flatten()
        .any(|version| sizes.get(&version.object).copied().unwrap_or(0) > MAX_DISPLAY_BYTES);
    if too_large {
        return Plan::Done(FileDiff::TooLarge);
    }
    if changed_lines > MAX_EXPANDED_CHANGES && !expanded {
        return Plan::Done(FileDiff::Collapsed);
    }
    Plan::Read
}

fn text_diff(
    change: &FileChange,
    hunks: Vec<DiffHunk>,
    blobs: &HashMap<String, Vec<u8>>,
) -> Result<FileDiff> {
    let text = |version: &Option<FileVersion>| -> Result<Option<Option<FileText>>> {
        let Some(version) = version else {
            return Ok(Some(None));
        };
        // Blobs are looked up, not taken: one blob can appear in several files.
        let bytes = blobs.get(&version.object).ok_or_else(|| {
            Error::Git(format!("blob {} was not read", short_sha(&version.object)))
        })?;
        if anchor::is_binary(bytes) {
            return Ok(None);
        }
        Ok(Some(Some(FileText {
            blob: version.object.clone(),
            lines: anchor::split_lines(bytes),
        })))
    };
    let (Some(old), Some(new)) = (text(&change.old)?, text(&change.new)?) else {
        return Ok(FileDiff::Binary);
    };
    Ok(FileDiff::Text { hunks, old, new })
}
