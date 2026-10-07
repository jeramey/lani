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

db_enum!(IssueStatus { Open => "open", Resolved => "resolved" });

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
        IssueResolved => "issue_resolved",
        IssueReopened => "issue_reopened",
        CommentAdded => "comment_added",
        CommentEdited => "comment_edited",
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
}

#[derive(Clone, Debug)]
pub struct Anchor {
    /// The commit whose content the issue's author read; line numbers refer to it.
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
    pub status: IssueStatus,
    pub resolved_by: Option<Actor>,
    pub resolved_at: Option<String>,
    pub anchor: Anchor,
    pub created_at: String,
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
