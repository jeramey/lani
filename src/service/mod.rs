//! Domain operations shared by the MCP server and (from M2) the web server.
//!
//! Every business rule lives here once: review and issue lifecycles, permissions
//! (docs/design.md §11.2), anchor validation (§10.1), relocation (§10.2) and optimistic
//! concurrency (§11.4). Git commands never run while a database transaction is open.

mod code;

pub use code::{DirectoryEntry, FileContent, MAX_DISPLAY_BYTES, PathContent, PathView};

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, TransactionBehavior};

use crate::anchor::{self, DiskCheck, FileLines, Relocation};
use crate::base::{self, BaseChoice, BaseMode};
use crate::error::{Error, Result};
use crate::git::{self, Git, PathChange, Worktree};
use crate::model::{
    Actor, Anchor, BaseSource, Comment, Event, EventKind, Issue, IssueStatus, Repo, Review,
    ReviewStatus, Side, short_ref, short_sha,
};
use crate::settings::{self, IdleTimeout};
use crate::store::{self, IssueCounts, NewComment, NewEvent, NewReview};
use crate::text::{self, quoted};
use crate::time;

/// Longest accepted review title, in characters.
const MAX_TITLE_CHARS: usize = 200;
/// How many recent events the activity timeline shows.
pub const TIMELINE_EVENTS: u32 = 50;

pub struct Service {
    conn: Connection,
}

// ── request types ────────────────────────────────────────────────────────────────────────────

pub struct StartReview<'a> {
    /// Any path inside the worktree the review is started from.
    pub repo_path: &'a Path,
    pub title: &'a str,
    pub author: Actor,
    pub base_mode: BaseMode,
    pub summary: Option<&'a str>,
    /// Branch to review; defaults to the branch checked out at `repo_path`.
    pub head_ref: Option<&'a str>,
}

pub struct CreateIssue<'a> {
    pub review_id: i64,
    pub author: Actor,
    pub body: &'a str,
    pub path: &'a str,
    pub side: Side,
    pub start_line: u32,
    pub end_line: u32,
    /// Required from MCP; `None` only for the web UI, whose lines come from a rendered commit.
    pub start_line_text: Option<&'a str>,
    /// The commit whose content the author read.
    pub commit: &'a str,
    /// The worktree the author read the file from, or `None` if it was read from git.
    pub worktree_path: Option<&'a Path>,
}

#[derive(Clone, Copy, Debug)]
pub enum CommentTarget {
    Issue(i64),
    /// The review's summary thread.
    Summary(i64),
}

// ── result types ─────────────────────────────────────────────────────────────────────────────

/// A worktree's state compared with a review's head.
#[derive(Clone, Debug)]
pub struct WorktreeReport {
    pub path: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub matches_review_head: bool,
    pub modified: Vec<String>,
}

/// Where a review's refs point right now.
#[derive(Clone, Debug)]
pub struct ReviewGitState {
    /// `None` if the head branch no longer exists.
    pub head_sha: Option<String>,
    /// `None` for a whole-tree review, or if the base ref no longer exists.
    pub base_sha: Option<String>,
    pub merge_base: Option<String>,
    /// Commits on the head since the merge-base.
    pub commits_ahead: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct ReviewView {
    pub review: Review,
    pub repo: Repo,
    pub counts: IssueCounts,
    /// Present when the caller needs current git state (not in review lists) and the
    /// repository still exists.
    pub git: Option<ReviewGitState>,
    /// Why git state is missing although it was requested: the repository is no longer where
    /// LANI recorded it. Everything stored in the database is still shown.
    pub repo_unavailable: Option<String>,
}

#[derive(Clone, Debug)]
pub struct StartedReview {
    /// `false` when an open review already existed for the branch and was returned as-is.
    pub created: bool,
    pub view: ReviewView,
    /// How the base was chosen; only for newly created reviews.
    pub base_choice: Option<BaseChoice>,
    pub worktree: WorktreeReport,
}

#[derive(Clone, Debug)]
pub struct ReviewDetail {
    pub view: ReviewView,
    pub summary_comments: Vec<Comment>,
    pub worktree: Option<WorktreeReport>,
}

#[derive(Clone, Debug)]
pub struct ReviewListItem {
    pub view: ReviewView,
    /// Whether this review's head branch is checked out in the worktree the list was filtered by.
    pub checked_out: bool,
}

#[derive(Clone, Debug)]
pub struct ReviewList {
    /// The worktree the list was filtered by, if any.
    pub filtered_by: Option<PathBuf>,
    pub items: Vec<ReviewListItem>,
}

/// Where an issue's anchored lines are in the current code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Location {
    /// The lines exist unchanged at `commit` (the current head, or merge-base for side `base`).
    Current {
        commit: String,
        path: String,
        start_line: u32,
        end_line: u32,
    },
    /// The lines cannot be placed in the current code; `reason` says why.
    Outdated { reason: String },
}

/// Code shown with an issue.
#[derive(Clone, Debug)]
pub struct CodeExcerpt {
    /// The commit the lines are from.
    pub commit: String,
    pub first_line: u32,
    pub lines: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct IssueView {
    pub issue: Issue,
    pub location: Location,
    /// Oldest first; the first comment is the issue's description.
    pub comments: Vec<Comment>,
    /// Current code when the location is current, otherwise the stored snippet.
    /// `None` when not requested.
    pub code: Option<CodeExcerpt>,
}

#[derive(Clone, Debug)]
pub struct IssueList {
    pub view: ReviewView,
    pub issues: Vec<IssueView>,
    /// Issues left out by the status filter: in the filtered file when a path was given,
    /// otherwise in the whole review.
    pub not_shown: IssueCounts,
}

// ── operations ───────────────────────────────────────────────────────────────────────────────

impl Service {
    pub fn open(db_path: &Path) -> Result<Self> {
        Ok(Self {
            conn: store::open(db_path)?,
        })
    }

    /// Start a review of a branch, or return the branch's open review if it has one.
    pub fn start_review(&mut self, request: &StartReview<'_>) -> Result<StartedReview> {
        let title = validate_title(request.title)?;
        let worktree = git::discover(request.repo_path)?;
        let git = Git::new(&worktree.git_dir);

        let head_ref = head_branch(&git, &worktree, request.head_ref)?;
        let head_sha = git.resolve_commit(&head_ref)?.ok_or_else(|| {
            Error::Invalid(format!(
                "branch {} has no commits yet",
                quoted(short_ref(&head_ref))
            ))
        })?;

        let repo = store::upsert_repo(
            &self.conn,
            &path_text(&worktree.git_dir)?,
            &repo_name(&worktree.git_dir),
            &time::now(),
        )?;
        let worktree_report = worktree_report(&worktree, Some(&head_sha))?;

        if let Some(existing) = store::find_open_review(&self.conn, repo.id, &head_ref)? {
            return Ok(StartedReview {
                created: false,
                view: self.review_view(existing, true)?,
                base_choice: None,
                worktree: worktree_report,
            });
        }

        let base_choice = base::choose_base(&git, &head_ref, &head_sha, &request.base_mode)?;
        let summary = normalize_newlines(request.summary.unwrap_or(""));
        let now = time::now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let inserted = store::insert_review(
            &tx,
            &NewReview {
                repo_id: repo.id,
                title: &title,
                head_ref: &head_ref,
                base_ref: base_choice.base_ref.as_deref(),
                base_source: base_choice.source,
                created_by: request.author,
                summary: &summary,
                now: &now,
            },
        );
        let review_id = match inserted {
            Ok(id) => id,
            Err(error) if is_constraint_violation(&error) => {
                // Another session started a review of this branch between our check and insert.
                drop(tx);
                let existing =
                    store::find_open_review(&self.conn, repo.id, &head_ref)?.ok_or(error)?;
                return Ok(StartedReview {
                    created: false,
                    view: self.review_view(existing, true)?,
                    base_choice: None,
                    worktree: worktree_report,
                });
            }
            Err(error) => return Err(error),
        };
        store::insert_event(
            &tx,
            &NewEvent {
                review_id,
                issue_id: None,
                comment_id: None,
                kind: EventKind::ReviewCreated,
                actor: request.author,
                now: &now,
            },
        )?;
        tx.commit()?;

        let review = store::get_review(&self.conn, review_id)?;
        Ok(StartedReview {
            created: true,
            view: self.review_view(review, true)?,
            base_choice: Some(base_choice),
            worktree: worktree_report,
        })
    }

    /// List reviews, newest first, optionally only those of the repo containing `repo_path`.
    pub fn list_reviews(
        &self,
        status: Option<ReviewStatus>,
        repo_path: Option<&Path>,
    ) -> Result<ReviewList> {
        let Some(path) = repo_path else {
            let items = store::list_reviews(&self.conn, status, None)?
                .into_iter()
                .map(|review| {
                    Ok(ReviewListItem {
                        view: self.review_view(review, false)?,
                        checked_out: false,
                    })
                })
                .collect::<Result<_>>()?;
            return Ok(ReviewList {
                filtered_by: None,
                items,
            });
        };

        let worktree = git::discover(path)?;
        let filtered_by = Some(worktree.toplevel.clone());
        let Some(repo) = store::find_repo_by_git_dir(&self.conn, &path_text(&worktree.git_dir)?)?
        else {
            return Ok(ReviewList {
                filtered_by,
                items: Vec::new(),
            });
        };
        let checked_out_branch = git::current_branch(&worktree.toplevel)?;
        let items = store::list_reviews(&self.conn, status, Some(repo.id))?
            .into_iter()
            .map(|review| {
                let checked_out = checked_out_branch.as_deref() == Some(review.head_ref.as_str());
                Ok(ReviewListItem {
                    view: self.review_view(review, false)?,
                    checked_out,
                })
            })
            .collect::<Result<_>>()?;
        Ok(ReviewList { filtered_by, items })
    }

    /// A review with its current git state, summary comments and, if given, a worktree's state.
    pub fn get_review(&self, review_id: i64, worktree_path: Option<&Path>) -> Result<ReviewDetail> {
        let view = self.review_view(store::get_review(&self.conn, review_id)?, true)?;
        let worktree = match worktree_path {
            Some(path) => {
                let worktree = discover_in_repo(path, &view.repo, review_id)?;
                let head_sha = view.git.as_ref().and_then(|git| git.head_sha.as_deref());
                Some(worktree_report(&worktree, head_sha)?)
            }
            None => None,
        };
        Ok(ReviewDetail {
            summary_comments: store::summary_comments(&self.conn, review_id)?,
            view,
            worktree,
        })
    }

    /// Replace a review's summary if nobody changed it since `expected_version`.
    pub fn update_summary(
        &mut self,
        review_id: i64,
        actor: Actor,
        summary: &str,
        expected_version: i64,
    ) -> Result<ReviewDetail> {
        let review = store::get_review(&self.conn, review_id)?;
        require_open(&review)?;
        let summary = normalize_newlines(summary);
        if summary == review.summary {
            // Saving unchanged text is not an edit: no new version, event or "edited" marker.
            return self.get_review(review_id, None);
        }
        let now = time::now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !store::update_summary(&tx, review_id, &summary, actor, &now, expected_version)? {
            let current = store::get_review(&tx, review_id)?;
            return Err(summary_conflict(&current, expected_version));
        }
        store::insert_event(
            &tx,
            &NewEvent {
                review_id,
                issue_id: None,
                comment_id: None,
                kind: EventKind::SummaryUpdated,
                actor,
                now: &now,
            },
        )?;
        tx.commit()?;
        self.get_review(review_id, None)
    }

    /// A review's issues by issue number, oldest first, placed in the current code.
    pub fn list_issues(
        &self,
        review_id: i64,
        status: Option<IssueStatus>,
        path: Option<&str>,
        with_code: bool,
    ) -> Result<IssueList> {
        let view = self.review_view(store::get_review(&self.conn, review_id)?, true)?;
        let mut placer = Placer::new(&view);
        let mut issues = Vec::new();
        let mut not_shown = IssueCounts::default();
        for issue in store::list_issues(&self.conn, review_id)? {
            let location = match path {
                // Placing an issue finds the file its lines are in now, which may have been
                // renamed since it was opened; the filter matches either name.
                Some(path) => {
                    let location = placer.place(&issue.anchor);
                    if !is_in_file(&issue.anchor, &location, path) {
                        continue;
                    }
                    Some(location)
                }
                None => None,
            };
            if status.is_some_and(|wanted| issue.status != wanted) {
                match issue.status {
                    IssueStatus::Open => not_shown.open += 1,
                    IssueStatus::Resolved => not_shown.resolved += 1,
                }
                continue;
            }
            let location = location.unwrap_or_else(|| placer.place(&issue.anchor));
            issues.push(self.placed_issue_view(&mut placer, issue, location, with_code)?);
        }
        Ok(IssueList {
            view,
            issues,
            not_shown,
        })
    }

    /// One comment as stored.
    pub fn get_comment(&self, comment_id: i64) -> Result<Comment> {
        store::get_comment(&self.conn, comment_id)
    }

    /// An issue's stored record, without placing it in the current code (no git work).
    pub fn issue_record(&self, issue_id: i64) -> Result<Issue> {
        store::get_issue(&self.conn, issue_id)
    }

    /// One issue with its full thread and code.
    pub fn get_issue(&self, issue_id: i64) -> Result<IssueView> {
        let issue = store::get_issue(&self.conn, issue_id)?;
        let view = self.review_view(store::get_review(&self.conn, issue.review_id)?, true)?;
        let mut placer = Placer::new(&view);
        self.issue_view(&mut placer, issue, true)
    }

    /// Open an issue anchored to lines the author read at `request.commit` (§10.1).
    pub fn create_issue(&mut self, request: &CreateIssue<'_>) -> Result<IssueView> {
        let review = store::get_review(&self.conn, request.review_id)?;
        require_open(&review)?;
        let body = require_text("body", request.body)?;
        let repo = store::get_repo(&self.conn, review.repo_id)?;
        require_repo(&repo)?;
        let git = Git::new(&repo.git_dir);
        let head_sha = require_head(&git, &review)?;
        let anchor = validate_anchor(&git, &repo, &review, &head_sha, request)?;

        let now = time::now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let issue_id = store::insert_issue(&tx, review.id, request.author, &anchor, &now)?;
        let comment_id = store::insert_comment(
            &tx,
            &NewComment {
                review_id: review.id,
                issue_id: Some(issue_id),
                author: request.author,
                body: &body,
                head_sha: &head_sha,
                now: &now,
            },
        )?;
        store::insert_event(
            &tx,
            &NewEvent {
                review_id: review.id,
                issue_id: Some(issue_id),
                comment_id: Some(comment_id),
                kind: EventKind::IssueCreated,
                actor: request.author,
                now: &now,
            },
        )?;
        tx.commit()?;
        self.get_issue(issue_id)
    }

    /// Add a comment to an issue or to a review's summary thread.
    pub fn add_comment(
        &mut self,
        actor: Actor,
        target: CommentTarget,
        body: &str,
    ) -> Result<Comment> {
        let body = require_text("body", body)?;
        let (review_id, issue_id) = match target {
            CommentTarget::Issue(issue_id) => (
                store::get_issue(&self.conn, issue_id)?.review_id,
                Some(issue_id),
            ),
            CommentTarget::Summary(review_id) => (review_id, None),
        };
        let review = store::get_review(&self.conn, review_id)?;
        require_open(&review)?;
        let repo = store::get_repo(&self.conn, review.repo_id)?;
        require_repo(&repo)?;
        let head_sha = require_head(&Git::new(&repo.git_dir), &review)?;

        let now = time::now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let comment_id = store::insert_comment(
            &tx,
            &NewComment {
                review_id,
                issue_id,
                author: actor,
                body: &body,
                head_sha: &head_sha,
                now: &now,
            },
        )?;
        store::insert_event(
            &tx,
            &NewEvent {
                review_id,
                issue_id,
                comment_id: Some(comment_id),
                kind: EventKind::CommentAdded,
                actor,
                now: &now,
            },
        )?;
        tx.commit()?;
        store::get_comment(&self.conn, comment_id)
    }

    /// Replace a comment's body if nobody changed it since `expected_version`.
    /// The human may edit any comment; Claude only comments written by its own role.
    pub fn edit_comment(
        &mut self,
        actor: Actor,
        comment_id: i64,
        body: &str,
        expected_version: i64,
    ) -> Result<Comment> {
        let body = require_text("body", body)?;
        let comment = store::get_comment(&self.conn, comment_id)?;
        if actor != Actor::Human && actor != comment.author {
            return Err(Error::Forbidden(format!(
                "comment c{comment_id} was written by the {}; only that role or the human can edit it",
                comment.author.as_str()
            )));
        }
        require_open(&store::get_review(&self.conn, comment.review_id)?)?;
        if body == comment.body {
            // Saving unchanged text is not an edit: no new version, event or "edited" marker.
            return Ok(comment);
        }

        let now = time::now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !store::update_comment_body(&tx, comment_id, &body, actor, &now, expected_version)? {
            let current = store::get_comment(&tx, comment_id)?;
            return Err(comment_conflict(&current, expected_version));
        }
        store::insert_event(
            &tx,
            &NewEvent {
                review_id: comment.review_id,
                issue_id: comment.issue_id,
                comment_id: Some(comment_id),
                kind: EventKind::CommentEdited,
                actor,
                now: &now,
            },
        )?;
        tx.commit()?;
        store::get_comment(&self.conn, comment_id)
    }

    /// Resolve or reopen an issue. Only the reviewer and the human may do this (§11.2).
    pub fn set_issue_status(
        &mut self,
        actor: Actor,
        issue_id: i64,
        status: IssueStatus,
        comment: Option<&str>,
    ) -> Result<IssueView> {
        if actor == Actor::Implementer {
            return Err(Error::Forbidden(
                "implementers cannot resolve or reopen issues; the reviewer and the human do. \
                 Reply with add_comment to explain your fix instead."
                    .to_string(),
            ));
        }
        let issue = store::get_issue(&self.conn, issue_id)?;
        let review = store::get_review(&self.conn, issue.review_id)?;
        require_open(&review)?;
        if issue.status == status {
            return Err(Error::Invalid(format!(
                "issue {issue_id} is already {}",
                status.as_str()
            )));
        }
        let comment = comment
            .map(|text| require_text("comment", text))
            .transpose()?;
        if status == IssueStatus::Open && actor != Actor::Human && comment.is_none() {
            return Err(Error::Invalid(
                "reopening an issue needs a comment explaining why".to_string(),
            ));
        }
        // Only a comment records the head commit; changing the status needs no git, so issues
        // can still be resolved after the repository is moved or deleted.
        let head_sha = match &comment {
            Some(_) => {
                let repo = store::get_repo(&self.conn, review.repo_id)?;
                require_repo(&repo)?;
                Some(require_head(&Git::new(&repo.git_dir), &review)?)
            }
            None => None,
        };

        let now = time::now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let (Some(body), Some(head_sha)) = (&comment, &head_sha) {
            let comment_id = store::insert_comment(
                &tx,
                &NewComment {
                    review_id: review.id,
                    issue_id: Some(issue_id),
                    author: actor,
                    body,
                    head_sha,
                    now: &now,
                },
            )?;
            store::insert_event(
                &tx,
                &NewEvent {
                    review_id: review.id,
                    issue_id: Some(issue_id),
                    comment_id: Some(comment_id),
                    kind: EventKind::CommentAdded,
                    actor,
                    now: &now,
                },
            )?;
        }
        let (resolved_by, kind) = match status {
            IssueStatus::Resolved => (Some(actor), EventKind::IssueResolved),
            IssueStatus::Open => (None, EventKind::IssueReopened),
        };
        store::set_issue_status(&tx, issue_id, status, resolved_by, &now)?;
        store::insert_event(
            &tx,
            &NewEvent {
                review_id: review.id,
                issue_id: Some(issue_id),
                comment_id: None,
                kind,
                actor,
                now: &now,
            },
        )?;
        tx.commit()?;
        self.get_issue(issue_id)
    }

    /// Close a review. Only the reviewer and the human may do this (§11.2).
    pub fn close_review(&mut self, actor: Actor, review_id: i64) -> Result<ReviewView> {
        if actor == Actor::Implementer {
            return Err(Error::Forbidden(
                "implementers cannot close reviews; the reviewer and the human do".to_string(),
            ));
        }
        let review = store::get_review(&self.conn, review_id)?;
        require_open(&review)?;
        let now = time::now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        store::close_review(&tx, review_id, &now)?;
        store::insert_event(
            &tx,
            &NewEvent {
                review_id,
                issue_id: None,
                comment_id: None,
                kind: EventKind::ReviewClosed,
                actor,
                now: &now,
            },
        )?;
        tx.commit()?;
        self.review_view(store::get_review(&self.conn, review_id)?, false)
    }

    /// Reopen a closed review. Only the human may do this (§11.2).
    pub fn reopen_review(&mut self, actor: Actor, review_id: i64) -> Result<ReviewView> {
        require_human(actor, "reopen reviews")?;
        let review = store::get_review(&self.conn, review_id)?;
        if review.status == ReviewStatus::Open {
            return Err(Error::Invalid(format!(
                "review {review_id} is already open"
            )));
        }
        let now = time::now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        match store::reopen_review(&tx, review_id) {
            Ok(()) => {}
            Err(error) if is_constraint_violation(&error) => {
                let other = store::find_open_review(&tx, review.repo_id, &review.head_ref)?
                    .map_or_else(String::new, |open| format!(" (review {})", open.id));
                return Err(Error::Invalid(format!(
                    "branch {} already has an open review{other}; close it before reopening review {review_id}",
                    quoted(short_ref(&review.head_ref))
                )));
            }
            Err(error) => return Err(error),
        }
        store::insert_event(
            &tx,
            &NewEvent {
                review_id,
                issue_id: None,
                comment_id: None,
                kind: EventKind::ReviewReopened,
                actor,
                now: &now,
            },
        )?;
        tx.commit()?;
        self.review_view(store::get_review(&self.conn, review_id)?, true)
    }

    /// Rename a review. Only the human may do this (§11.2).
    pub fn retitle_review(
        &mut self,
        actor: Actor,
        review_id: i64,
        title: &str,
    ) -> Result<ReviewView> {
        require_human(actor, "rename reviews")?;
        let title = validate_title(title)?;
        let review = store::get_review(&self.conn, review_id)?;
        require_open(&review)?;
        if review.title == title {
            return self.review_view(review, true);
        }
        let now = time::now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        store::update_review_title(&tx, review_id, &title)?;
        store::insert_event(
            &tx,
            &NewEvent {
                review_id,
                issue_id: None,
                comment_id: None,
                kind: EventKind::ReviewRetitled,
                actor,
                now: &now,
            },
        )?;
        tx.commit()?;
        self.review_view(store::get_review(&self.conn, review_id)?, true)
    }

    /// Change a review's base to another ref (`BaseMode::Ref`) or to none, making it a
    /// whole-tree review (`BaseMode::None`). Only the human may do this, in the web UI (§9.5).
    /// Issues are anchored to commits, so they need no changes.
    pub fn change_base(
        &mut self,
        actor: Actor,
        review_id: i64,
        base: &BaseMode,
    ) -> Result<ReviewView> {
        require_human(actor, "change a review's base")?;
        let review = store::get_review(&self.conn, review_id)?;
        require_open(&review)?;
        let repo = store::get_repo(&self.conn, review.repo_id)?;
        let git = Git::new(&repo.git_dir);
        let base_ref = match base {
            BaseMode::Auto => {
                return Err(Error::Invalid(
                    "choose a base branch, or none for a whole-tree review".to_string(),
                ));
            }
            BaseMode::None => None,
            // Re-saving the current base is a no-op, even when its ref no longer resolves.
            BaseMode::Ref(name) if review.base_ref.as_deref() == Some(name.as_str()) => {
                return self.review_view(review, true);
            }
            BaseMode::Ref(_) => {
                require_repo(&repo)?;
                let head_sha = require_head(&git, &review)?;
                base::choose_base(&git, &review.head_ref, &head_sha, base)?.base_ref
            }
        };
        if base_ref == review.base_ref {
            return self.review_view(review, true);
        }
        let now = time::now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        store::set_review_base(&tx, review_id, base_ref.as_deref(), BaseSource::WebUi)?;
        store::insert_event(
            &tx,
            &NewEvent {
                review_id,
                issue_id: None,
                comment_id: None,
                kind: EventKind::ReviewBaseChanged,
                actor,
                now: &now,
            },
        )?;
        tx.commit()?;
        self.review_view(store::get_review(&self.conn, review_id)?, true)
    }

    /// Refs a review's base can be changed to: local and remote-tracking branches other than
    /// the review's head, as full ref names.
    pub fn base_candidates(&self, review_id: i64) -> Result<Vec<String>> {
        let review = store::get_review(&self.conn, review_id)?;
        let repo = store::get_repo(&self.conn, review.repo_id)?;
        require_repo(&repo)?;
        let refs = Git::new(&repo.git_dir).list_refs(&["refs/heads/", "refs/remotes/"])?;
        Ok(refs
            .into_iter()
            .filter(|name| *name != review.head_ref && !name.ends_with("/HEAD"))
            .collect())
    }

    /// A review's most recent activity, newest first.
    pub fn list_events(&self, review_id: i64) -> Result<Vec<Event>> {
        store::list_events(&self.conn, review_id, TIMELINE_EVENTS)
    }

    /// The saved idle timeout for the web server.
    pub fn idle_timeout(&self) -> Result<IdleTimeout> {
        let value =
            store::get_setting(&self.conn, settings::IDLE_TIMEOUT_KEY)?.ok_or_else(|| {
                Error::NotFound("the idle_timeout setting is missing from the database".to_string())
            })?;
        IdleTimeout::parse(&value)
    }

    /// Save the idle timeout. Only the human may change settings.
    pub fn set_idle_timeout(&mut self, actor: Actor, timeout: IdleTimeout) -> Result<()> {
        require_human(actor, "change settings")?;
        store::set_setting(&self.conn, settings::IDLE_TIMEOUT_KEY, &timeout.to_string())
    }

    fn review_view(&self, review: Review, with_git: bool) -> Result<ReviewView> {
        let repo = store::get_repo(&self.conn, review.repo_id)?;
        let counts = store::issue_counts(&self.conn, review.id)?;
        // A missing repository is expected (moved or deleted) and degrades to stored data;
        // any other git failure is a real error and is reported.
        let (git, repo_unavailable) = if !with_git {
            (None, None)
        } else if repo_exists(&repo) {
            (
                Some(review_git_state(&Git::new(&repo.git_dir), &review)?),
                None,
            )
        } else {
            (None, Some(repo_missing(&repo)))
        };
        Ok(ReviewView {
            review,
            repo,
            counts,
            git,
            repo_unavailable,
        })
    }

    fn issue_view(
        &self,
        placer: &mut Placer<'_>,
        issue: Issue,
        with_code: bool,
    ) -> Result<IssueView> {
        let location = placer.place(&issue.anchor);
        self.placed_issue_view(placer, issue, location, with_code)
    }

    /// An issue's view given where `placer` placed it.
    fn placed_issue_view(
        &self,
        placer: &mut Placer<'_>,
        issue: Issue,
        location: Location,
        with_code: bool,
    ) -> Result<IssueView> {
        let comments = store::issue_comments(&self.conn, issue.id)?;
        let code = if with_code {
            Some(placer.excerpt(&issue.anchor, &location))
        } else {
            None
        };
        Ok(IssueView {
            issue,
            location,
            comments,
            code,
        })
    }
}

// ── issue placement ──────────────────────────────────────────────────────────────────────────

/// Places anchors in a review's current code, caching git results for one request.
struct Placer<'a> {
    git: Git,
    review: &'a Review,
    state: Option<&'a ReviewGitState>,
    /// Set when the repository is gone, so every anchor is shown from its stored snippet.
    repo_unavailable: Option<&'a str>,
    /// Changed paths per (anchor commit, target): one whole-tree diff per pair of commits.
    path_changes: HashMap<(String, String), HashMap<String, PathChange>>,
    /// Whether each anchor commit still exists.
    commits_exist: HashMap<String, bool>,
    files: HashMap<(String, String), Option<FileLines>>,
}

impl<'a> Placer<'a> {
    fn new(view: &'a ReviewView) -> Self {
        Self {
            git: Git::new(&view.repo.git_dir),
            review: &view.review,
            state: view.git.as_ref(),
            repo_unavailable: view.repo_unavailable.as_deref(),
            path_changes: HashMap::new(),
            commits_exist: HashMap::new(),
            files: HashMap::new(),
        }
    }

    /// Where the anchor's lines are now. Git failures degrade to an outdated location whose
    /// reason carries the error, so one broken issue cannot hide the others.
    fn place(&mut self, anchor: &Anchor) -> Location {
        self.try_place(anchor)
            .unwrap_or_else(|error| outdated(format!("could not follow the lines: {error}")))
    }

    fn try_place(&mut self, anchor: &Anchor) -> Result<Location> {
        if let Some(reason) = self.repo_unavailable {
            return Ok(outdated(reason.to_string()));
        }
        let target = match anchor.side {
            Side::Head => match self.state.and_then(|s| s.head_sha.clone()) {
                Some(head) => head,
                None => {
                    return Ok(outdated(format!(
                        "head branch {} no longer exists",
                        quoted(short_ref(&self.review.head_ref))
                    )));
                }
            },
            Side::Base => match self.state.and_then(|s| s.merge_base.clone()) {
                Some(merge_base) => merge_base,
                None if self.review.base_ref.is_none() => {
                    return Ok(outdated("the review no longer has a base".to_string()));
                }
                None => return Ok(outdated("the review's base cannot be resolved".to_string())),
            },
        };
        let current = |path: &str, start_line: u32, end_line: u32| Location::Current {
            commit: target.clone(),
            path: path.to_string(),
            start_line,
            end_line,
        };
        if anchor.commit == target {
            return Ok(current(&anchor.path, anchor.start_line, anchor.end_line));
        }
        let exists = if let Some(exists) = self.commits_exist.get(&anchor.commit) {
            *exists
        } else {
            let exists = self.git.resolve_commit(&anchor.commit)?.is_some();
            self.commits_exist.insert(anchor.commit.clone(), exists);
            exists
        };
        if !exists {
            return Ok(outdated(format!(
                "commit {} is no longer in the repository",
                short_sha(&anchor.commit)
            )));
        }
        let pair = (anchor.commit.clone(), target.clone());
        if !self.path_changes.contains_key(&pair) {
            let changes = self.git.path_changes(&anchor.commit, &target)?;
            self.path_changes.insert(pair.clone(), changes);
        }
        let change = self.path_changes[&pair]
            .get(&anchor.path)
            .cloned()
            .unwrap_or(PathChange::Unchanged);
        let new_path = match change {
            PathChange::Unchanged => {
                return Ok(current(&anchor.path, anchor.start_line, anchor.end_line));
            }
            PathChange::Deleted => {
                return Ok(outdated(format!(
                    "{} was removed after {}",
                    quoted(&anchor.path),
                    short_sha(&anchor.commit)
                )));
            }
            PathChange::Modified => anchor.path.clone(),
            PathChange::Renamed(new_path) => new_path,
        };
        let hunks = self
            .git
            .diff_hunks(&anchor.commit, &target, &anchor.path, &new_path)?;
        Ok(
            match anchor::relocate(&hunks, anchor.start_line, anchor.end_line) {
                Relocation::Moved {
                    start_line,
                    end_line,
                } => current(&new_path, start_line, end_line),
                Relocation::Changed => outdated(format!(
                    "the lines changed between {} and {}",
                    short_sha(&anchor.commit),
                    short_sha(&target)
                )),
            },
        )
    }

    /// The code to show for an issue: current lines with context, or the stored snippet.
    fn excerpt(&mut self, anchor: &Anchor, location: &Location) -> CodeExcerpt {
        if let Location::Current {
            commit,
            path,
            start_line,
            end_line,
        } = location
            && let Some(file) = self.file(commit, path)
            && *end_line <= file.len()
        {
            let (first_line, lines) = file.excerpt(*start_line, *end_line);
            return CodeExcerpt {
                commit: commit.clone(),
                first_line,
                lines: lines.into_iter().map(str::to_string).collect(),
            };
        }
        CodeExcerpt {
            commit: anchor.commit.clone(),
            first_line: anchor.snippet_start,
            lines: anchor.snippet.split('\n').map(str::to_string).collect(),
        }
    }

    fn file(&mut self, commit: &str, path: &str) -> Option<&FileLines> {
        let key = (commit.to_string(), path.to_string());
        if !self.files.contains_key(&key) {
            let file = self
                .git
                .read_file(commit, path)
                .ok()
                .flatten()
                .and_then(|bytes| FileLines::from_bytes(path, &bytes).ok());
            self.files.insert(key.clone(), file);
        }
        self.files.get(&key).and_then(Option::as_ref)
    }
}

fn outdated(reason: String) -> Location {
    Location::Outdated { reason }
}

// ── helpers ──────────────────────────────────────────────────────────────────────────────────

fn review_git_state(git: &Git, review: &Review) -> Result<ReviewGitState> {
    let head_sha = git.resolve_commit(&review.head_ref)?;
    let base_sha = match &review.base_ref {
        Some(base_ref) => git.resolve_commit(base_ref)?,
        None => None,
    };
    let (merge_base, commits_ahead) = match (&head_sha, &base_sha) {
        (Some(head), Some(base)) => match git.merge_base(base, head)? {
            Some(merge_base) => {
                let ahead = git.commit_count(&merge_base, head)?;
                (Some(merge_base), Some(ahead))
            }
            None => (None, None),
        },
        _ => (None, None),
    };
    Ok(ReviewGitState {
        head_sha,
        base_sha,
        merge_base,
        commits_ahead,
    })
}

fn worktree_report(worktree: &Worktree, review_head: Option<&str>) -> Result<WorktreeReport> {
    let state = git::worktree_state(&worktree.toplevel)?;
    Ok(WorktreeReport {
        path: worktree.toplevel.clone(),
        matches_review_head: state.head.is_some() && state.head.as_deref() == review_head,
        head: state.head,
        branch: state.branch,
        modified: state.modified,
    })
}

/// Whether an issue is in file `path`: the file it was opened on, or the file its lines are in
/// now after a rename.
fn is_in_file(anchor: &Anchor, location: &Location, path: &str) -> bool {
    anchor.path == path
        || matches!(location, Location::Current { path: current, .. } if current == path)
}

/// The full ref of the branch a new review covers: `head_ref` if given, otherwise the branch
/// checked out in `worktree`.
fn head_branch(git: &Git, worktree: &Worktree, head_ref: Option<&str>) -> Result<String> {
    match head_ref {
        Some(name) if git::is_worktree_relative(name) => Err(Error::Invalid(format!(
            "head_ref {} is relative to a worktree's HEAD or reflog, which LANI would read from \
             the repository's main worktree, not yours. Pass the branch name, or leave head_ref \
             out to review the branch checked out at repo_path.",
            quoted(name)
        ))),
        Some(name) => match git.full_ref_name(name)? {
            Some(full) if full.starts_with("refs/heads/") => Ok(full),
            _ => Err(Error::Invalid(format!(
                "head_ref {name:?} is not a local branch in this repository"
            ))),
        },
        None => git::current_branch(&worktree.toplevel)?.ok_or_else(|| {
            Error::Invalid(format!(
                "HEAD is detached in {}; check out the branch to review, or pass head_ref",
                quoted(&worktree.toplevel.display().to_string())
            ))
        }),
    }
}

/// Find the worktree containing `path` and require it to belong to `repo`.
fn discover_in_repo(path: &Path, repo: &Repo, review_id: i64) -> Result<Worktree> {
    let worktree = git::discover(path)?;
    if path_text(&worktree.git_dir)? != repo.git_dir {
        return Err(Error::Invalid(format!(
            "{} belongs to a different repository than review {review_id} ({})",
            quoted(&path.display().to_string()),
            quoted(&repo.name)
        )));
    }
    Ok(worktree)
}

/// Validate a new issue's anchor (§10.1 steps 1–4) and capture its snippet.
fn validate_anchor(
    git: &Git,
    repo: &Repo,
    review: &Review,
    head_sha: &str,
    request: &CreateIssue<'_>,
) -> Result<Anchor> {
    anchor::validate_path(request.path)?;
    if request.start_line == 0 || request.end_line < request.start_line {
        return Err(Error::Invalid(format!(
            "invalid line range {}-{}: lines start at 1 and end_line must not be less than start_line",
            request.start_line, request.end_line
        )));
    }
    if request.side == Side::Base && request.worktree_path.is_some() {
        return Err(Error::Invalid(
            "worktree_path must be null for side \"base\": the base version of a file only exists in git"
                .to_string(),
        ));
    }

    // Step 1: the commit the author read is on the branch. It must be given as a SHA: LANI
    // resolves names in the common git dir, where `HEAD`, `@{-1}` and other per-worktree names
    // mean the main worktree's, so from a linked worktree they could name a commit nobody read.
    if !is_object_name(request.commit) {
        return Err(Error::Invalid(format!(
            "commit {} is not a commit SHA. Pass the SHA that `git rev-parse HEAD` prints in the \
             worktree you read, not a name such as HEAD: LANI would look a name up in the \
             repository's main worktree, which may have a different commit checked out.",
            quoted(request.commit)
        )));
    }
    let commit = git.resolve_commit(request.commit)?.ok_or_else(|| {
        Error::NotFound(format!(
            "commit {:?} does not exist in this repository",
            request.commit
        ))
    })?;
    if !git.is_ancestor(&commit, head_sha)? {
        return Err(Error::Invalid(format!(
            "commit {} is not on branch {} (head {}). Either you read a different branch, or the branch \
             was rebased since you read it; re-read the code at the current head and retry.",
            short_sha(&commit),
            quoted(short_ref(&review.head_ref)),
            short_sha(head_sha)
        )));
    }

    // Step 2: the path and range exist in the anchor commit.
    let anchor_commit = anchor_commit(git, review, request.side, &commit)?;
    let content = git
        .read_file(&anchor_commit, request.path)?
        .ok_or_else(|| {
            Error::NotFound(format!(
                "{} does not exist at commit {}",
                quoted(request.path),
                short_sha(&anchor_commit)
            ))
        })?;
    let file = FileLines::from_bytes(request.path, &content)?;
    if request.end_line > file.len() {
        return Err(Error::Invalid(format!(
            "{} has {} lines at commit {}, so lines {}-{} do not exist",
            quoted(request.path),
            file.len(),
            short_sha(&anchor_commit),
            request.start_line,
            request.end_line
        )));
    }

    // Step 3: the file on disk matches the commit around the range.
    if let Some(worktree_path) = request.worktree_path {
        let worktree = discover_in_repo(worktree_path, repo, review.id)?;
        check_file_on_disk(&worktree, &commit, request)?;
    }

    // Step 4: the first line's text matches.
    if let Some(expected) = request.start_line_text {
        check_start_line_text(&file, request, expected, &anchor_commit)?;
    }

    let (snippet_start, snippet_lines) = file.excerpt(request.start_line, request.end_line);
    Ok(Anchor {
        commit: anchor_commit,
        path: request.path.to_string(),
        side: request.side,
        start_line: request.start_line,
        end_line: request.end_line,
        snippet: snippet_lines.join("\n"),
        snippet_start,
    })
}

/// Whether `commit` is a full or abbreviated object name (4 to 64 hex digits), as
/// `git rev-parse HEAD` prints it, rather than a name that git resolves.
fn is_object_name(commit: &str) -> bool {
    (4..=64).contains(&commit.len()) && commit.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// The commit an anchor's line numbers refer to: the commit read, or for side `base` its
/// merge-base with the review's base branch.
fn anchor_commit(git: &Git, review: &Review, side: Side, commit: &str) -> Result<String> {
    match side {
        Side::Head => Ok(commit.to_string()),
        Side::Base => {
            let base_ref = review.base_ref.as_deref().ok_or_else(|| {
                Error::Invalid(format!(
                    "review {} is a whole-tree review, so it has no base side; use side \"head\"",
                    review.id
                ))
            })?;
            let base_sha = git.resolve_commit(base_ref)?.ok_or_else(|| {
                Error::NotFound(format!(
                    "base branch {} no longer exists",
                    quoted(short_ref(base_ref))
                ))
            })?;
            git.merge_base(&base_sha, commit)?.ok_or_else(|| {
                Error::Invalid(format!(
                    "commit {} shares no history with base {}",
                    short_sha(commit),
                    quoted(short_ref(base_ref))
                ))
            })
        }
    }
}

/// §10.1 step 3: reject anchors that uncommitted changes could have shifted.
fn check_file_on_disk(worktree: &Worktree, commit: &str, request: &CreateIssue<'_>) -> Result<()> {
    let path = request.path;
    let shown_path = quoted(path);
    let on_disk = worktree.toplevel.join(path);
    if std::fs::symlink_metadata(&on_disk).is_err() {
        return Err(Error::Invalid(format!(
            "{shown_path} does not exist on disk in {}; if you read it from git (git show), pass worktree_path: null",
            quoted(&worktree.toplevel.display().to_string())
        )));
    }
    let hunks = git::diff_commit_to_disk(&worktree.toplevel, commit, path)?;
    let (start, end) = (request.start_line, request.end_line);
    let disk = quoted(&worktree.toplevel.display().to_string());
    let check = anchor::check_disk(&hunks, start, end);
    if check == DiskCheck::Consistent {
        return Ok(());
    }
    // The diff is against the files on disk, so with another commit checked out the
    // differences may be committed; then the likely mistake is the commit passed.
    let worktree_head = git::worktree_head(&worktree.toplevel)?;
    if worktree_head.as_deref() != Some(commit) {
        let (checked_out, if_read_from_disk) = match &worktree_head {
            Some(head) => (
                format!("has commit {} checked out", short_sha(head)),
                format!(
                    "resubmit with commit {head}, the SHA that `git rev-parse HEAD` prints there."
                ),
            ),
            None => (
                "has no commit checked out (its branch has no commits yet)".to_string(),
                "it is not committed, so it cannot be reviewed until it is.".to_string(),
            ),
        };
        let commit = short_sha(commit);
        return Err(Error::Invalid(format!(
            "create_issue rejected: {shown_path} on disk in {disk} differs from commit {commit} in a way that \
             affects lines {start}-{end}, and that worktree {checked_out}, so the differences may be committed.\n  \
             - If you read the file on disk: {if_read_from_disk}\n  \
             - If your line numbers come from commit {commit} (git show, or the worktree before it moved on): \
             resubmit with worktree_path: null."
        )));
    }
    let commit = short_sha(commit);
    match check {
        DiskCheck::Consistent => Ok(()),
        DiskCheck::RangeChanged => Err(Error::Invalid(format!(
            "create_issue rejected: lines {start}-{end} of {shown_path} on disk in {disk} differ from commit {commit}.\n\
             Uncommitted changes are not part of the review; they must be committed to be reviewed."
        ))),
        DiskCheck::Shifted { if_read_from_disk } => {
            let from_disk = match if_read_from_disk {
                Relocation::Moved {
                    start_line,
                    end_line,
                } => format!(
                    "the same lines are {start_line}-{end_line} in {commit}. Resubmit with those numbers."
                ),
                Relocation::Changed => {
                    "those lines have uncommitted changes, so they cannot be reviewed until committed."
                        .to_string()
                }
            };
            Err(Error::Invalid(format!(
                "create_issue rejected: {shown_path} on disk in {disk} differs from commit {commit}.\n\
                 The differences are above lines {start}-{end}, so LANI cannot tell which version your line numbers came from.\n  \
                 - If you read the file on disk: {from_disk}\n  \
                 - If you read the committed file (git show): resubmit with worktree_path: null.\n\
                 Uncommitted changes are not part of the review; they must be committed to be reviewed."
            )))
        }
    }
}

/// §10.1 step 4: the first anchored line must match what the author says it is.
fn check_start_line_text(
    file: &FileLines,
    request: &CreateIssue<'_>,
    expected: &str,
    anchor_commit: &str,
) -> Result<()> {
    let actual = file.line(request.start_line).unwrap_or_default();
    if actual.trim() == expected.trim() {
        return Ok(());
    }
    let matches = file.find(expected);
    let suggestion = match matches.as_slice() {
        [] => "No line in the file has that text.".to_string(),
        lines => {
            let shown: Vec<String> = lines.iter().take(5).map(u32::to_string).collect();
            format!("Lines with that text: {}.", shown.join(", "))
        }
    };
    Err(Error::Invalid(format!(
        "start_line_text does not match line {} of {} at {}.\n  line {}: {:?}\n  expected: {:?}\n{suggestion}",
        request.start_line,
        quoted(request.path),
        short_sha(anchor_commit),
        request.start_line,
        actual,
        expected
    )))
}

fn repo_exists(repo: &Repo) -> bool {
    Path::new(&repo.git_dir).is_dir()
}

fn repo_missing(repo: &Repo) -> String {
    format!(
        "the repository is no longer at {} (it was moved or deleted)",
        quoted(&repo.git_dir)
    )
}

/// Fail with a clear message when an operation needs the repository and it is gone.
fn require_repo(repo: &Repo) -> Result<()> {
    if repo_exists(repo) {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "{}; this needs the repository's current commits",
            repo_missing(repo)
        )))
    }
}

fn require_human(actor: Actor, what: &str) -> Result<()> {
    if actor == Actor::Human {
        Ok(())
    } else {
        Err(Error::Forbidden(format!(
            "only the human can {what}, in the web UI"
        )))
    }
}

fn require_open(review: &Review) -> Result<()> {
    match review.status {
        ReviewStatus::Open => Ok(()),
        ReviewStatus::Closed => Err(Error::Invalid(format!(
            "review {} is closed; the human can reopen it in the web UI",
            review.id
        ))),
    }
}

/// The review's current head commit; new activity needs one to record.
fn require_head(git: &Git, review: &Review) -> Result<String> {
    git.resolve_commit(&review.head_ref)?.ok_or_else(|| {
        Error::Invalid(format!(
            "head branch {} of review {} no longer exists",
            quoted(short_ref(&review.head_ref)),
            review.id
        ))
    })
}

/// Normalize line endings to `\n` and require some non-blank text.
fn require_text(field: &str, text: &str) -> Result<String> {
    let text = normalize_newlines(text);
    if text.trim().is_empty() {
        Err(Error::Invalid(format!("{field} must not be empty")))
    } else {
        Ok(text)
    }
}

/// Convert `\r\n` and lone `\r` line endings to `\n`. Browsers submit `<textarea>` text with
/// `\r\n` line endings, so without this every web edit would change the stored text even when
/// the human changed nothing, and Claude would read the `\r`s back.
fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn validate_title(title: &str) -> Result<String> {
    let title = title.trim();
    if title.is_empty() {
        return Err(Error::Invalid("title must not be empty".to_string()));
    }
    if title
        .chars()
        .any(|c| text::is_line_break(c) || c.is_control())
    {
        return Err(Error::Invalid(
            "title must be a single line without control characters".to_string(),
        ));
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(Error::Invalid(format!(
            "title must be at most {MAX_TITLE_CHARS} characters"
        )));
    }
    Ok(title.to_string())
}

fn summary_conflict(current: &Review, expected_version: i64) -> Error {
    let editor = current.summary_updated_by.map_or("nobody", Actor::as_str);
    Error::Conflict(format!(
        "the summary changed after version {expected_version}: it is now version {} (last edited by {editor}).\n\
         {}Apply your change to this text (without the prefixes) and pass expected_version {}.",
        current.summary_version,
        quoted_current_text(&current.summary),
        current.summary_version
    ))
}

fn comment_conflict(current: &Comment, expected_version: i64) -> Error {
    let editor = current.edited_by.unwrap_or(current.author).as_str();
    Error::Conflict(format!(
        "comment c{} changed after version {expected_version}: it is now version {} (last edited by {editor}).\n\
         {}Apply your change to this text (without the prefixes) and pass expected_version {}.",
        current.id,
        current.version,
        quoted_current_text(&current.body),
        current.version
    ))
}

/// Text someone wrote, embedded in a conflict error. It is quoted line by line like comments in
/// MCP output, so it cannot pass for LANI's own lines or a message from the human (§12.4).
fn quoted_current_text(current: &str) -> String {
    if current.trim().is_empty() {
        "The current text is empty.\n".to_string()
    } else {
        format!(
            "Current text, each line shown after a `{}` prefix that is not part of it:\n{}",
            text::BODY_PREFIX.trim_end(),
            text::quote_body(current)
        )
    }
}

fn is_constraint_violation(error: &Error) -> bool {
    matches!(
        error,
        Error::Db(rusqlite::Error::SqliteFailure(failure, _))
            if failure.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

/// The display name of a repo: its main worktree's directory name.
fn repo_name(git_dir: &Path) -> String {
    let name = if git_dir.file_name().is_some_and(|name| name == ".git") {
        git_dir.parent().and_then(Path::file_name)
    } else {
        git_dir.file_name()
    };
    let name = name.map_or_else(
        || git_dir.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    name.strip_suffix(".git")
        .map(str::to_string)
        .unwrap_or(name)
}

fn path_text(path: &Path) -> Result<String> {
    path.to_str().map(str::to_string).ok_or_else(|| {
        Error::Invalid(format!(
            "path {} is not valid UTF-8",
            quoted(&path.display().to_string())
        ))
    })
}
