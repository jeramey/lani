//! Domain operations shared by the MCP server and (from M2) the web server.
//!
//! Every business rule lives here once: review and issue lifecycles, permissions
//! (docs/design.md §11.2), anchor validation (§10.1), relocation (§10.2) and optimistic
//! concurrency (§11.4). Git commands never run while a database transaction is open.

mod code;

pub use code::{
    ChangedFile, ChangesView, DirectoryEntry, FileContent, FileDiff, FileText, MAX_DISPLAY_BYTES,
    MAX_EXPANDED_CHANGES, PathContent, PathView, review_changes,
};

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::anchor::{self, DiskCheck, FileLines, Relocation};
use crate::base::{self, BaseChoice, BaseMode};
use crate::error::{Error, Result};
use crate::git::{self, Git, PathChange, Worktree};
use crate::model::{
    Actor, Anchor, BaseSource, Comment, Event, EventKind, Issue, IssueStatus, PromptKind, Repo,
    Review, ReviewStatus, Severity, Side, short_ref, short_sha,
};
use crate::prompt;
use crate::settings::{self, IdleTimeout};
use crate::store::{self, IssueCounts, NewComment, NewEvent, NewPromptEdit, NewReview};
use crate::text::{self, quoted};
use crate::time;

/// Longest accepted review title, in characters.
const MAX_TITLE_CHARS: usize = 200;

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
    /// One line naming the problem.
    pub title: &'a str,
    pub severity: Severity,
    pub body: &'a str,
    /// The line ranges the issue is about: 1 to [`MAX_SPANS`], in the author's order.
    pub spans: Vec<SpanRequest<'a>>,
    /// The commit whose content the author read.
    pub commit: &'a str,
    /// The worktree the author read the file from, or `None` if it was read from git.
    pub worktree_path: Option<&'a Path>,
    /// For side `base` from the web UI: the merge-base the page showed. If the base has moved
    /// since, the merge-base and its line numbers may differ, so the issue is rejected. `None`
    /// skips the check.
    pub read_merge_base: Option<&'a str>,
    /// Ask the human about the issue as it is filed: its body is the question (§11.5). Only the
    /// reviewer and the implementer ask.
    pub ask_human: bool,
}

/// The most spans one issue can have.
pub const MAX_SPANS: usize = 20;

/// One line range of a new issue (§10.1).
#[derive(Clone, Copy, Debug)]
pub struct SpanRequest<'a> {
    pub path: &'a str,
    pub side: Side,
    pub start_line: u32,
    pub end_line: u32,
    /// Required from MCP; `None` only for the web UI, whose lines come from a rendered commit.
    pub start_line_text: Option<&'a str>,
}

/// Which page of a review's activity to show (§13.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActivityRequest {
    /// Events per page; `None` for all on one page.
    pub per_page: Option<NonZeroU32>,
    /// The page, from 1, the newest.
    pub page: NonZeroU32,
    /// The review's latest event when the reader paged back. Pages count back from it, so they
    /// stay put while new events arrive; `None` counts from the latest event, so page 1 always
    /// shows the newest.
    pub anchor: Option<i64>,
}

/// A page of a review's activity, newest first.
#[derive(Clone, Debug)]
pub struct ActivityPage {
    pub events: Vec<Event>,
    /// How many newer events the pages before this one hold.
    pub offset: u64,
    /// How many events the pages cover: those up to the anchor, or all of them.
    pub total: u64,
    /// Events after the anchor, which no page shows until the reader returns to the latest; 0
    /// without an anchor.
    pub newer: u64,
    /// The review's latest event, the anchor for paging back from a page without one.
    pub latest_id: i64,
}

/// The result of handing a review on.
#[derive(Clone, Debug)]
pub struct HandOff {
    /// The review after the change.
    pub review: Review,
    /// Set when LANI sent the review to the human instead of the role named.
    pub redirected: Option<Redirect>,
    pub round_limit: u32,
}

/// Why a hand-off went to the human instead of the role named (§11.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Redirect {
    /// The reviewer's first hand-off on a review, so the human reads the first findings before
    /// the implementer does.
    FirstFindings,
    /// The implementer's hand-back reached the round limit, given here.
    RoundLimit(u32),
}

/// How waiting for a turn ended.
#[derive(Clone, Debug)]
pub enum WaitOutcome {
    YourTurn(Box<ReviewView>),
    Closed(Review),
    TimedOut(Review),
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
    /// Issues with an open question for the human, by number (§11.5).
    pub questions: Vec<store::Question>,
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
    /// Every change of turn, with its note, newest first. The newest is the current turn.
    pub hand_offs: Vec<Event>,
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
    /// Where each span is now, in the order of `issue.anchors`.
    pub locations: Vec<Location>,
    /// Oldest first; the first comment is the issue's description.
    pub comments: Vec<Comment>,
    /// Each span's code: current lines when its location is current, otherwise its stored
    /// snippet. `None` when not requested.
    pub code: Option<Vec<CodeExcerpt>>,
}

impl IssueView {
    /// Each span with where it is now.
    pub fn spans(&self) -> impl Iterator<Item = (&Anchor, &Location)> {
        self.issue.anchors.iter().zip(&self.locations)
    }
}

#[derive(Clone, Debug)]
pub struct IssueList {
    pub view: ReviewView,
    pub issues: Vec<IssueView>,
    /// Issues left out by the status filter: in the filtered file when a path was given,
    /// otherwise in the whole review.
    pub not_shown: IssueCounts,
}

/// A reviewer prompt for the review page (§13.9): its default, and the text the page shows, which
/// is the human's last copied edit when there is one.
#[derive(Clone, Debug)]
pub struct ReviewerPrompt {
    pub kind: PromptKind,
    pub default: String,
    pub text: String,
    /// Why the page shows the default although the human's last copy was an edit.
    pub notice: Option<PromptNotice>,
}

/// Why the review page shows a prompt's default instead of the human's last copied edit (§13.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptNotice {
    /// The edit was made from another default: the review's base or LANI changed since. It would
    /// name the old base, or miss improvements, so it stays in the history only.
    DefaultChanged,
    /// The edit's stored diff no longer applies to its default, as after a hand edit of the
    /// database. A side panel must not take the review's page down, so the page shows the default.
    Unreadable,
}

impl ReviewerPrompt {
    #[must_use]
    pub fn edited(&self) -> bool {
        self.text != self.default
    }
}

// ── operations ───────────────────────────────────────────────────────────────────────────────

impl Service {
    pub fn open(db_path: &Path) -> Result<Self> {
        Ok(Self {
            conn: store::open(db_path)?,
        })
    }

    /// Fail with [`Error::Outdated`] once a newer lani has upgraded the database since this
    /// process opened it. Long-running processes call this before each request; every write also
    /// checks again inside its transaction (§7.4).
    pub fn check_schema(&self) -> Result<()> {
        store::check_schema(&self.conn)
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

        let tx = store::write_transaction(&mut self.conn)?;
        let repo = store::upsert_repo(
            &tx,
            &path_text(&worktree.git_dir)?,
            &repo_name(&worktree.git_dir),
            &time::now(),
        )?;
        tx.commit()?;
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
        let tx = store::write_transaction(&mut self.conn)?;
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
            hand_offs: store::list_hand_offs(&self.conn, review_id)?,
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
        let tx = store::write_transaction(&mut self.conn)?;
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
            let locations = match path {
                // Placing an issue finds the files its lines are in now, which may have been
                // renamed since it was opened; the filter matches either name of any span.
                Some(path) => {
                    let locations = placer.place_all(&issue.anchors);
                    let in_file = issue
                        .anchors
                        .iter()
                        .zip(&locations)
                        .any(|(anchor, location)| is_in_file(anchor, location, path));
                    if !in_file {
                        continue;
                    }
                    Some(locations)
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
            let locations = locations.unwrap_or_else(|| placer.place_all(&issue.anchors));
            issues.push(self.placed_issue_view(&mut placer, issue, locations, with_code)?);
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
        if request.ask_human && request.author == Actor::Human {
            return Err(Error::Invalid(
                "the human answers questions; only the reviewer and the implementer ask them"
                    .to_string(),
            ));
        }
        let review = store::get_review(&self.conn, request.review_id)?;
        require_open(&review)?;
        let title = validate_title(request.title)?;
        let body = require_text("body", request.body)?;
        let repo = store::get_repo(&self.conn, review.repo_id)?;
        require_repo(&repo)?;
        let git = Git::new(&repo.git_dir);
        let head_sha = require_head(&git, &review)?;
        let anchors = validate_anchors(&git, &repo, &review, &head_sha, request)?;

        let now = time::now();
        let tx = store::write_transaction(&mut self.conn)?;
        let issue_id = store::insert_issue(
            &tx,
            &store::NewIssue {
                review_id: review.id,
                author: request.author,
                title: &title,
                severity: request.severity,
                anchors: &anchors,
                now: &now,
            },
        )?;
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
        if request.ask_human {
            store::set_asked(&tx, issue_id, Some(request.author), &now)?;
            store::insert_event(
                &tx,
                &NewEvent {
                    review_id: review.id,
                    issue_id: Some(issue_id),
                    comment_id: Some(comment_id),
                    kind: EventKind::HumanAsked,
                    actor: request.author,
                    now: &now,
                },
            )?;
        }
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
        self.post_comment(actor, target, body, false)
    }

    /// Ask the human about an issue: add `body` as a comment and mark the issue as waiting for
    /// the human until they reply on it or resolve it (§11.5). Only Claude asks, and only on an
    /// open issue.
    pub fn ask_human(&mut self, actor: Actor, issue_id: i64, body: &str) -> Result<Comment> {
        if actor == Actor::Human {
            return Err(Error::Invalid(
                "the human answers questions; only the reviewer and the implementer ask them"
                    .to_string(),
            ));
        }
        self.post_comment(actor, CommentTarget::Issue(issue_id), body, true)
    }

    fn post_comment(
        &mut self,
        actor: Actor,
        target: CommentTarget,
        body: &str,
        ask_human: bool,
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
        let tx = store::write_transaction(&mut self.conn)?;
        // A question has to be answered before its issue is resolved (§11.5), so a resolved issue
        // takes none. Read in the write transaction, so a resolve made at the same moment is seen.
        if ask_human
            && let Some(issue_id) = issue_id
            && store::get_issue(&tx, issue_id)?.status == IssueStatus::Resolved
        {
            return Err(Error::Invalid(format!(
                "issue {issue_id} is resolved, and a question for the human must be answered \
                 before its issue is resolved. Ask on an open issue, or have the issue reopened \
                 first: the reviewer reopens with set_issue_status."
            )));
        }
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
        if let Some(issue_id) = issue_id {
            if ask_human {
                store::set_asked(&tx, issue_id, Some(actor), &now)?;
            } else if actor == Actor::Human {
                // The human's reply answers any question about the issue.
                store::set_asked(&tx, issue_id, None, &now)?;
            }
        }
        store::insert_event(
            &tx,
            &NewEvent {
                review_id,
                issue_id,
                comment_id: Some(comment_id),
                kind: if ask_human {
                    EventKind::HumanAsked
                } else {
                    EventKind::CommentAdded
                },
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
        let tx = store::write_transaction(&mut self.conn)?;
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

    /// Change an issue's title and severity. Only the human does this; Claude sets them when it
    /// opens the issue (§11.2).
    pub fn edit_issue(
        &mut self,
        actor: Actor,
        issue_id: i64,
        title: &str,
        severity: Severity,
        expected_version: i64,
    ) -> Result<Issue> {
        require_human(actor, "change an issue's title or severity")?;
        let title = validate_title(title)?;
        let issue = store::get_issue(&self.conn, issue_id)?;
        require_open(&store::get_review(&self.conn, issue.review_id)?)?;
        if title == issue.title && severity == issue.severity {
            // Saving unchanged values is not an edit: no new version, event or "edited" marker.
            return Ok(issue);
        }

        let now = time::now();
        let tx = store::write_transaction(&mut self.conn)?;
        if !store::update_issue_details(
            &tx,
            issue_id,
            &title,
            severity,
            actor,
            &now,
            expected_version,
        )? {
            let current = store::get_issue(&tx, issue_id)?;
            return Err(Error::Conflict(format!(
                "issue {issue_id} changed after version {expected_version}: it is now version {}, \
                 titled {} with severity {}. Apply your change to it and pass version {}.",
                current.version,
                quoted(&current.title),
                current.severity.as_str(),
                current.version
            )));
        }
        store::insert_event(
            &tx,
            &NewEvent {
                review_id: issue.review_id,
                issue_id: Some(issue_id),
                comment_id: None,
                kind: EventKind::IssueEdited,
                actor,
                now: &now,
            },
        )?;
        tx.commit()?;
        store::get_issue(&self.conn, issue_id)
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
        let tx = store::write_transaction(&mut self.conn)?;
        // A question is settled by the human: their reply, or their own resolve, which acknowledges
        // the outcome. Nobody resolves the issue for them meanwhile (§11.5). Read in the write
        // transaction, so a question asked at the same moment is seen.
        if status == IssueStatus::Resolved
            && actor != Actor::Human
            && let Some(asker) = store::get_issue(&tx, issue_id)?.asked_by
        {
            return Err(Error::Forbidden(format!(
                "issue {issue_id} has a question for the human, asked by the {}, so only the \
                 human can resolve it until they answer. If the human asked you to resolve it, \
                 tell them to reply on the issue, or resolve it themselves, in the web UI, so \
                 that the review keeps their answer.",
                asker.as_str()
            )));
        }
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
        if actor == Actor::Human {
            // The human's resolve answers any question about the issue, acknowledging the outcome
            // (§11.5). A question's issue is always open, so a reopen meets one only if it was
            // left on a resolved issue from before that rule; acting on the issue answers it too.
            store::set_asked(&tx, issue_id, None, &now)?;
        }
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

    /// Hand a review to `to`, or with `None` (the human only) to nobody (§11.5).
    ///
    /// The reviewer and the implementer hand on a review only when it is their turn or nobody's,
    /// to the other role or to the human. The human hands it to either role or to nobody, which
    /// starts the round count again. When the implementer hands back to the reviewer for the
    /// `round_limit`-th time since the human last handed it on, the review goes to the human
    /// instead.
    pub fn hand_off(
        &mut self,
        actor: Actor,
        review_id: i64,
        to: Option<Actor>,
        note: Option<&str>,
    ) -> Result<HandOff> {
        let note = note
            .map(normalize_newlines)
            .filter(|note| !note.trim().is_empty());
        // Everything the change depends on is read inside the write transaction, so a hand-off
        // made meanwhile by another session or the web UI is seen, not overwritten.
        let tx = store::write_transaction(&mut self.conn)?;
        let review = store::get_review(&tx, review_id)?;
        require_open(&review)?;
        if actor == Actor::Human {
            if to == Some(Actor::Human) {
                return Err(Error::Invalid(
                    "hand the review to the reviewer, the implementer, or nobody".to_string(),
                ));
            }
        } else {
            match to {
                None => {
                    return Err(Error::Invalid(
                        "hand the review to the other role or to the human; only the human \
                         clears the turn"
                            .to_string(),
                    ));
                }
                Some(target) if target == actor => {
                    return Err(Error::Invalid(format!(
                        "you are the {}; hand the review to the other role or to the human",
                        actor.as_str()
                    )));
                }
                Some(_) => {}
            }
            if let Some(holder) = review.turn
                && holder != actor
            {
                return Err(Error::Invalid(format!(
                    "it is the {}'s turn on review {review_id}, not yours; only the role whose turn \
                     it is can hand the review on. Wait for your turn with \
                     `lani wait --review {review_id} --for {}`.",
                    holder.as_str(),
                    actor.as_str()
                )));
            }
        }
        let limit = round_limit(&tx)?;
        let (to, rounds, redirected) = match (actor, to) {
            (Actor::Human, to) => (to, 0, None),
            // The human reads the reviewer's first findings before the implementer does.
            (Actor::Reviewer, Some(Actor::Implementer))
                if !store::has_handed_off(&tx, review_id, Actor::Reviewer)? =>
            {
                (
                    Some(Actor::Human),
                    review.rounds,
                    Some(Redirect::FirstFindings),
                )
            }
            // A round is the reviewer handing the review to the implementer and the implementer
            // handing it back; a loop the implementer opened, or the human handed it, starts none.
            (Actor::Implementer, Some(Actor::Reviewer))
                if review.turn_by == Some(Actor::Reviewer) =>
            {
                let rounds = review.rounds + 1;
                if rounds >= limit {
                    (
                        Some(Actor::Human),
                        rounds,
                        Some(Redirect::RoundLimit(limit)),
                    )
                } else {
                    (Some(Actor::Reviewer), rounds, None)
                }
            }
            (_, to) => (to, review.rounds, None),
        };
        // While a question waits for the human, the review goes only to them: nobody hands it on,
        // the human included, until each question has had their answer (§11.5). Checked on where
        // the review would go, so a redirect to the human goes through.
        if to != Some(Actor::Human)
            && let Some(refusal) = awaiting_answers(actor, &store::questions(&tx, review_id)?)
        {
            return Err(Error::Invalid(refusal));
        }
        let now = time::now();
        store::change_turn(
            &tx,
            &store::TurnChange {
                review_id,
                turn: to,
                by: actor,
                note: note.as_deref(),
                rounds,
                now: &now,
            },
        )?;
        tx.commit()?;
        Ok(HandOff {
            review: store::get_review(&self.conn, review_id)?,
            redirected,
            round_limit: limit,
        })
    }

    /// The round limit: how many times the implementer may hand a review back to the reviewer
    /// before it goes to the human (§11.5).
    pub fn round_limit(&self) -> Result<u32> {
        round_limit(&self.conn)
    }

    /// Save the round limit. Only the human may change settings.
    pub fn set_round_limit(&mut self, actor: Actor, limit: u32) -> Result<()> {
        require_human(actor, "change settings")?;
        let limit = settings::parse_round_limit(&limit.to_string())?;
        let tx = store::write_transaction(&mut self.conn)?;
        store::set_setting(&tx, settings::ROUND_LIMIT_KEY, &limit.to_string())?;
        Ok(tx.commit()?)
    }

    /// Wait until it is `role`'s turn on a review, checking every `poll`. Returns early when the
    /// review is closed, or when `timeout` passes.
    pub fn wait_for_turn(
        &self,
        review_id: i64,
        role: Actor,
        timeout: Option<std::time::Duration>,
        poll: std::time::Duration,
    ) -> Result<WaitOutcome> {
        let deadline = timeout.map(|timeout| std::time::Instant::now() + timeout);
        loop {
            store::check_schema(&self.conn)?;
            let review = store::get_review(&self.conn, review_id)?;
            if review.status == ReviewStatus::Closed {
                return Ok(WaitOutcome::Closed(review));
            }
            if review.turn == Some(role) {
                return Ok(WaitOutcome::YourTurn(Box::new(
                    self.review_view(review, false)?,
                )));
            }
            if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
                return Ok(WaitOutcome::TimedOut(review));
            }
            std::thread::sleep(poll);
        }
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
        let tx = store::write_transaction(&mut self.conn)?;
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
        let tx = store::write_transaction(&mut self.conn)?;
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
        let tx = store::write_transaction(&mut self.conn)?;
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
        let tx = store::write_transaction(&mut self.conn)?;
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

    /// The review's reviewer prompts, one of each kind, for the review page (§13.9).
    pub fn reviewer_prompts(&self, review_id: i64) -> Result<Vec<ReviewerPrompt>> {
        let review = store::get_review(&self.conn, review_id)?;
        [PromptKind::ClaudeCode, PromptKind::Generic]
            .into_iter()
            .map(|kind| {
                let default = prompt::default_text(kind, &review)?;
                let Shown { text, notice } = shown_prompt(&self.conn, review_id, kind, &default)?;
                Ok(ReviewerPrompt {
                    kind,
                    default,
                    text,
                    notice,
                })
            })
            .collect()
    }

    /// Record a reviewer prompt the human copied, when its text differs from what the review page
    /// shows for it now, or the page shows a notice: as a diff from the default, or as a return to
    /// the default (§13.9). Returns whether a revision was recorded.
    pub fn save_reviewer_prompt(
        &mut self,
        actor: Actor,
        review_id: i64,
        kind: PromptKind,
        text: &str,
    ) -> Result<bool> {
        require_human(actor, "edit reviewer prompts")?;
        let review = store::get_review(&self.conn, review_id)?;
        require_open(&review)?;
        let text = normalize_newlines(text);
        let default = prompt::default_text(kind, &review)?;
        let now = time::now();
        let tx = store::write_transaction(&mut self.conn)?;
        // Copying what the page shows records nothing, unless the page also says why it shows the
        // default: then the copy accepts the new default, and the notice goes.
        let shown = shown_prompt(&tx, review_id, kind, &default)?;
        if text == shown.text && shown.notice.is_none() {
            return Ok(false);
        }
        // Only now, so that copying what the page shows never depends on diffing it.
        let patch = if text == default {
            None
        } else {
            Some(prompt::diff(&default, &text)?)
        };
        store::insert_prompt_edit(
            &tx,
            &NewPromptEdit {
                review_id,
                kind,
                default_text: &default,
                patch: patch.as_deref(),
                lani_version: crate::daemon::VERSION,
                now: &now,
            },
        )?;
        tx.commit()?;
        Ok(true)
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

    /// All of a review's events, newest first.
    pub fn list_events(&self, review_id: i64) -> Result<Vec<Event>> {
        store::list_events(
            &self.conn,
            review_id,
            store::EventPage {
                upto: None,
                offset: 0,
                limit: None,
            },
        )
    }

    /// A page of a review's activity. Fails with `Invalid` for a page past the last, or an anchor
    /// before the review's first event.
    pub fn activity(&self, review_id: i64, request: ActivityRequest) -> Result<ActivityPage> {
        // One read transaction, so the events and the counts agree although other processes
        // write meanwhile.
        let tx = self.conn.unchecked_transaction()?;
        let latest_id = store::latest_event_id(&tx, review_id)?
            .ok_or_else(|| Error::NotFound(format!("review {review_id} does not exist")))?;
        let upto = request.anchor.unwrap_or(latest_id);
        let total = store::count_events_upto(&tx, review_id, upto)?;
        // Event ids are shared by all reviews, so an anchor can come before this review's first.
        if total == 0 {
            return Err(Error::Invalid(format!(
                "review {review_id} has no events up to event {upto}"
            )));
        }
        let newer = match request.anchor {
            Some(anchor) => store::count_events_after(&tx, review_id, anchor)?,
            None => 0,
        };
        let pages = request.per_page.map_or(1, |per_page| {
            total.div_ceil(u64::from(per_page.get())).max(1)
        });
        if u64::from(request.page.get()) > pages {
            return Err(Error::Invalid(format!(
                "page {} is past the last page of events ({pages})",
                request.page
            )));
        }
        let offset = request.per_page.map_or(0, |per_page| {
            u64::from(request.page.get() - 1) * u64::from(per_page.get())
        });
        let events = store::list_events(
            &tx,
            review_id,
            store::EventPage {
                upto: Some(upto),
                offset,
                limit: request.per_page.map(NonZeroU32::get),
            },
        )?;
        tx.commit()?;
        Ok(ActivityPage {
            events,
            offset,
            total,
            newer,
            latest_id,
        })
    }

    /// The id of the review's latest event. Every change to a review records an event, so the
    /// web UI's live updates watch this number (§13.5).
    pub fn latest_event_id(&self, review_id: i64) -> Result<i64> {
        store::latest_event_id(&self.conn, review_id)?
            .ok_or_else(|| Error::NotFound(format!("review {review_id} does not exist")))
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
        let tx = store::write_transaction(&mut self.conn)?;
        store::set_setting(&tx, settings::IDLE_TIMEOUT_KEY, &timeout.to_string())?;
        Ok(tx.commit()?)
    }

    fn review_view(&self, review: Review, with_git: bool) -> Result<ReviewView> {
        let repo = store::get_repo(&self.conn, review.repo_id)?;
        let counts = store::issue_counts(&self.conn, review.id)?;
        let questions = store::questions(&self.conn, review.id)?;
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
            questions,
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
        let locations = placer.place_all(&issue.anchors);
        self.placed_issue_view(placer, issue, locations, with_code)
    }

    /// An issue's view given where `placer` placed it.
    fn placed_issue_view(
        &self,
        placer: &mut Placer<'_>,
        issue: Issue,
        locations: Vec<Location>,
        with_code: bool,
    ) -> Result<IssueView> {
        let comments = store::issue_comments(&self.conn, issue.id)?;
        let code = with_code.then(|| {
            issue
                .anchors
                .iter()
                .zip(&locations)
                .map(|(anchor, location)| placer.excerpt(anchor, location))
                .collect()
        });
        Ok(IssueView {
            issue,
            locations,
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

    /// Where each of an issue's spans is now.
    fn place_all(&mut self, anchors: &[Anchor]) -> Vec<Location> {
        anchors.iter().map(|anchor| self.place(anchor)).collect()
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

/// Validate a new issue's spans (§10.1 steps 1–4) and capture their snippets.
fn validate_anchors(
    git: &Git,
    repo: &Repo,
    review: &Review,
    head_sha: &str,
    request: &CreateIssue<'_>,
) -> Result<Vec<Anchor>> {
    let spans = request.spans.as_slice();
    if spans.is_empty() || spans.len() > MAX_SPANS {
        return Err(Error::Invalid(format!(
            "an issue needs 1 to {MAX_SPANS} spans, not {}",
            spans.len()
        )));
    }
    let numbered = spans.len() > 1;
    for (number, span) in (1..).zip(spans) {
        check_span_shape(span).map_err(|error| in_span(error, number, span, numbered))?;
    }
    if request.worktree_path.is_some() && spans.iter().any(|span| span.side == Side::Base) {
        return Err(Error::Invalid(
            "worktree_path must be null when a span is on side \"base\": the base version of a \
             file only exists in git, so read every span with git show"
                .to_string(),
        ));
    }
    for (i, a) in spans.iter().enumerate() {
        for (j, b) in spans.iter().enumerate().skip(i + 1) {
            if a.path == b.path
                && a.side == b.side
                && a.start_line <= b.end_line
                && b.start_line <= a.end_line
            {
                return Err(Error::Invalid(format!(
                    "spans {} and {} overlap in {} ({}-{} and {}-{}); merge them into one span",
                    i + 1,
                    j + 1,
                    quoted(a.path),
                    a.start_line,
                    a.end_line,
                    b.start_line,
                    b.end_line
                )));
            }
        }
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
            "commit {} does not exist in this repository",
            quoted(request.commit)
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
    let worktree = match request.worktree_path {
        Some(path) => Some(discover_in_repo(path, repo, review.id)?),
        None => None,
    };

    // Steps 2 to 4, for each span.
    let read = Read {
        git,
        review,
        commit: &commit,
        worktree: worktree.as_ref(),
        shown_merge_base: request.read_merge_base,
    };
    (1..)
        .zip(spans)
        .map(|(number, span)| {
            validate_span(&read, span).map_err(|error| in_span(error, number, span, numbered))
        })
        .collect()
}

/// What every span of a new issue was read from.
struct Read<'a> {
    git: &'a Git,
    review: &'a Review,
    /// The full SHA of the commit the author read.
    commit: &'a str,
    worktree: Option<&'a Worktree>,
    /// For side `base` from the web UI: the merge-base the page showed.
    shown_merge_base: Option<&'a str>,
}

/// The checks a span passes on its own: a clean path and a line range starting at 1.
fn check_span_shape(span: &SpanRequest<'_>) -> Result<()> {
    anchor::validate_path(span.path)?;
    if span.start_line == 0 || span.end_line < span.start_line {
        return Err(Error::Invalid(format!(
            "invalid line range {}-{}: lines start at 1 and end_line must not be less than start_line",
            span.start_line, span.end_line
        )));
    }
    Ok(())
}

/// §10.1 steps 2 to 4 for one span: its lines exist, the file on disk matches, and its first
/// line is what the author says it is.
fn validate_span(read: &Read<'_>, span: &SpanRequest<'_>) -> Result<Anchor> {
    let anchor_commit = anchor_commit(read.git, read.review, span.side, read.commit)?;
    if let Some(shown) = read.shown_merge_base
        && span.side == Side::Base
        && shown != anchor_commit
    {
        return Err(Error::Invalid(format!(
            "the merge-base is now {}, not {} as when the lines were read: the base moved. \
             Reload the changes and select the lines again.",
            short_sha(&anchor_commit),
            short_sha(shown)
        )));
    }
    let content = read
        .git
        .read_file(&anchor_commit, span.path)?
        .ok_or_else(|| {
            Error::NotFound(format!(
                "{} does not exist at commit {}",
                quoted(span.path),
                short_sha(&anchor_commit)
            ))
        })?;
    let file = FileLines::from_bytes(span.path, &content)?;
    if span.end_line > file.len() {
        return Err(Error::Invalid(format!(
            "{} has {} lines at commit {}, so lines {}-{} do not exist",
            quoted(span.path),
            file.len(),
            short_sha(&anchor_commit),
            span.start_line,
            span.end_line
        )));
    }
    if let Some(worktree) = read.worktree {
        check_file_on_disk(worktree, read.commit, span)?;
    }
    if let Some(expected) = span.start_line_text {
        check_start_line_text(&file, span, expected, &anchor_commit)?;
    }
    let (snippet_start, snippet_lines) = file.excerpt(span.start_line, span.end_line);
    Ok(Anchor {
        commit: anchor_commit,
        path: span.path.to_string(),
        side: span.side,
        start_line: span.start_line,
        end_line: span.end_line,
        snippet: snippet_lines.join("\n"),
        snippet_start,
    })
}

/// Name the span a validation error is about, when the issue has more than one.
fn in_span(error: Error, number: usize, span: &SpanRequest<'_>, numbered: bool) -> Error {
    if !numbered {
        return error;
    }
    let prefix = format!(
        "span {number} ({}:{}-{}): ",
        quoted(span.path),
        span.start_line,
        span.end_line
    );
    match error {
        Error::Invalid(message) => Error::Invalid(prefix + &message),
        Error::NotFound(message) => Error::NotFound(prefix + &message),
        other => other,
    }
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
fn check_file_on_disk(worktree: &Worktree, commit: &str, request: &SpanRequest<'_>) -> Result<()> {
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
    request: &SpanRequest<'_>,
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

/// The saved round limit (§11.5).
fn round_limit(conn: &Connection) -> Result<u32> {
    let value = store::get_setting(conn, settings::ROUND_LIMIT_KEY)?.ok_or_else(|| {
        Error::NotFound("the round_limit setting is missing from the database".to_string())
    })?;
    settings::parse_round_limit(&value)
}

/// Why `actor`'s hand-off to anyone but the human is refused while `questions` wait for the
/// human's answer (§11.5), and what to do instead; `None` when no question waits. Issue numbers
/// only: titles would need quoting.
fn awaiting_answers(actor: Actor, questions: &[store::Question]) -> Option<String> {
    let numbers: Vec<String> = questions
        .iter()
        .map(|question| question.issue_id.to_string())
        .collect();
    let (last, rest) = numbers.split_last()?;
    let issues = if rest.is_empty() {
        format!("Issue {last} is")
    } else {
        format!("Issues {} and {last} are", rest.join(", "))
    };
    Some(if actor == Actor::Human {
        format!(
            "{issues} waiting for your answer. Until you answer, the review can only be handed to \
             you: reply on each issue, or resolve it."
        )
    } else {
        format!(
            "{issues} waiting for the human's answer, so the review can only be handed to the \
             human until they answer. Call hand_off with to \"human\", and tell them."
        )
    })
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

/// What the review page shows for a prompt, and why when it isn't the human's last copy.
struct Shown {
    text: String,
    notice: Option<PromptNotice>,
}

/// The text the review page shows for a prompt: the latest revision, or the current `default`
/// when there is none, the latest went back to the default, or the latest was made from another
/// default.
fn shown_prompt(
    conn: &Connection,
    review_id: i64,
    kind: PromptKind,
    default: &str,
) -> Result<Shown> {
    let the_default = |notice| Shown {
        text: default.to_string(),
        notice,
    };
    match store::latest_prompt_edit(conn, review_id, kind)? {
        Some(store::PromptRevision {
            default_text,
            patch: Some(patch),
        }) => {
            if default_text != default {
                return Ok(the_default(Some(PromptNotice::DefaultChanged)));
            }
            match prompt::apply(&default_text, &patch) {
                Ok(text) => Ok(Shown { text, notice: None }),
                Err(error) => {
                    tracing::warn!(review_id, kind = kind.as_str(), %error, "showing the default prompt");
                    Ok(the_default(Some(PromptNotice::Unreadable)))
                }
            }
        }
        Some(store::PromptRevision { patch: None, .. }) | None => Ok(the_default(None)),
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
