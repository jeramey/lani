//! Domain types stored in the database (see docs/design.md §4 and §7.3).

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};

/// Defines an enum whose variants are stored as fixed lowercase strings in the database.
macro_rules! db_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            #[must_use]
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }

            #[must_use]
            pub fn parse(text: &str) -> Option<Self> {
                match text {
                    $($text => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }

        impl ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                Ok(ToSqlOutput::from(self.as_str()))
            }
        }

        impl FromSql for $name {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                let text = value.as_str()?;
                Self::parse(text).ok_or_else(|| {
                    FromSqlError::Other(
                        format!(concat!("invalid ", stringify!($name), ": {:?}"), text).into(),
                    )
                })
            }
        }
    };
}

db_enum!(
    /// Who wrote or did something. The MCP interface can only act as `Reviewer` or `Implementer`.
    Actor {
        Human => "human",
        Reviewer => "reviewer",
        Implementer => "implementer",
    }
);

db_enum!(ReviewStatus { Open => "open", Closed => "closed" });

db_enum!(
    /// The kind of harness a reviewer prompt is written for (§13.8).
    PromptKind {
        ClaudeCode => "claude_code",
        Generic => "generic",
    }
);

db_enum!(IssueStatus { Open => "open", Resolved => "resolved" });

db_enum!(
    /// How much an issue matters, as its author judged it.
    Severity {
        High => "high",
        Medium => "medium",
        Low => "low",
        Nit => "nit",
    }
);

db_enum!(
    /// Which version of a file an issue's line numbers refer to: the head commit, or the
    /// merge-base with the review's base branch (for lines the branch deleted).
    Side {
        Head => "head",
        Base => "base",
    }
);

db_enum!(
    /// How a review's base was chosen.
    BaseSource {
        Auto => "auto",
        Explicit => "explicit",
        WebUi => "web_ui",
    }
);

db_enum!(
    EventKind {
        ReviewCreated => "review_created",
        ReviewRetitled => "review_retitled",
        ReviewBaseChanged => "review_base_changed",
        ReviewClosed => "review_closed",
        ReviewReopened => "review_reopened",
        SummaryUpdated => "summary_updated",
        IssueCreated => "issue_created",
        IssueEdited => "issue_edited",
        IssueResolved => "issue_resolved",
        IssueReopened => "issue_reopened",
        CommentAdded => "comment_added",
        CommentEdited => "comment_edited",
        TurnChanged => "turn_changed",
        HumanAsked => "human_asked",
    }
);

#[derive(Clone, Debug)]
pub struct Repo {
    pub id: i64,
    /// Canonical absolute path of the common git dir, shared by all worktrees.
    pub git_dir: String,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct Review {
    pub id: i64,
    pub repo_id: i64,
    pub title: String,
    /// Full ref name of the branch under review, e.g. `refs/heads/feature/x`.
    pub head_ref: String,
    /// Full ref name of the base; `None` for a whole-tree review.
    pub base_ref: Option<String>,
    pub base_source: BaseSource,
    pub status: ReviewStatus,
    pub created_by: Actor,
    pub created_at: String,
    pub closed_at: Option<String>,
    pub summary: String,
    pub summary_updated_by: Option<Actor>,
    pub summary_updated_at: Option<String>,
    pub summary_version: i64,
    /// Who should act on the review now; `None` for nobody (§11.5).
    pub turn: Option<Actor>,
    /// Who last changed the turn, and when.
    pub turn_by: Option<Actor>,
    pub turn_at: Option<String>,
    /// The note given with the last change of turn.
    pub turn_note: Option<String>,
    /// Times the implementer handed the review back to the reviewer since the human last handed
    /// it on.
    pub rounds: u32,
}

/// One span of an issue: a line range in one file, as its author read it (§10).
#[derive(Clone, Debug)]
pub struct Anchor {
    /// The commit the line numbers refer to: the commit the author read, or for side `base` its
    /// merge-base with the review's base branch.
    pub commit: String,
    pub path: String,
    pub side: Side,
    pub start_line: u32,
    pub end_line: u32,
    /// The anchored lines plus up to 3 context lines on each side, as they were at `commit`.
    pub snippet: String,
    /// Line number of the snippet's first line.
    pub snippet_start: u32,
}

#[derive(Clone, Debug)]
pub struct Issue {
    pub id: i64,
    pub review_id: i64,
    pub author: Actor,
    /// One line naming the problem, shown in lists.
    pub title: String,
    pub severity: Severity,
    pub status: IssueStatus,
    pub resolved_by: Option<Actor>,
    pub resolved_at: Option<String>,
    /// The issue's spans, never empty, in the order its author gave them.
    pub anchors: Vec<Anchor>,
    pub created_at: String,
    /// Who last changed the title or severity, if anyone did after creation.
    pub edited_by: Option<Actor>,
    pub edited_at: Option<String>,
    /// The version of the title and severity, for optimistic concurrency (§11.4).
    pub version: i64,
    /// The role that asked the human about this issue, while the question is open (§11.5).
    pub asked_by: Option<Actor>,
    pub asked_at: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Comment {
    pub id: i64,
    pub review_id: i64,
    /// `None` for a comment on the review summary.
    pub issue_id: Option<i64>,
    pub author: Actor,
    pub body: String,
    /// The review's head commit when the comment was written.
    pub head_sha: String,
    pub created_at: String,
    pub edited_by: Option<Actor>,
    pub edited_at: Option<String>,
    pub version: i64,
}

/// One entry in a review's append-only activity log.
#[derive(Clone, Debug)]
pub struct Event {
    pub id: i64,
    pub review_id: i64,
    pub issue_id: Option<i64>,
    pub comment_id: Option<i64>,
    pub kind: EventKind,
    pub actor: Actor,
    /// For `TurnChanged`: who has the turn now; `None` for nobody.
    pub turn_to: Option<Actor>,
    /// For `TurnChanged`: the note given with the hand-off, if any.
    pub note: Option<String>,
    pub created_at: String,
}

/// Shorten a full ref name for display: `refs/heads/main` → `main`,
/// `refs/remotes/origin/main` → `origin/main`. Other refs are shown in full.
#[must_use]
pub fn short_ref(full: &str) -> &str {
    full.strip_prefix("refs/heads/")
        .or_else(|| full.strip_prefix("refs/remotes/"))
        .unwrap_or(full)
}

/// Abbreviate a commit SHA for display.
#[must_use]
pub fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}
