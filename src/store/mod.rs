//! SQLite storage: connection setup, migrations and typed queries (docs/design.md §7).
//!
//! Functions take `&Connection`; a `rusqlite::Transaction` dereferences to one, so the same
//! functions work inside and outside transactions. No git command runs while a transaction is
//! open (§7.2).

use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};

use crate::error::{Error, Result};
use crate::model::{
    Actor, Anchor, BaseSource, Comment, Event, EventKind, Issue, IssueStatus, Repo, Review,
    ReviewStatus, Severity,
};

/// Migrations in order. The first creates the baseline schema at [`BASELINE_VERSION`]; each
/// later one brings the schema to the next `user_version`.
const MIGRATIONS: &[&str] = &[
    include_str!("migrations/0002_baseline.sql"),
    include_str!("migrations/0003_turns.sql"),
    include_str!("migrations/0004_hand_off_notes.sql"),
];

/// The schema version the baseline migration creates. Databases at an earlier version come from
/// development builds before the schema was restructured; they are refused, not upgraded.
const BASELINE_VERSION: i64 = 2;

/// Open (creating if needed) the database at `path` and bring its schema up to date.
pub fn open(path: &Path) -> Result<Connection> {
    // Create the file with mode 0600 before SQLite opens it; SQLite gives the -wal and -shm
    // files the same mode as the database file.
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;

    let mut conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    let journal_mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(Error::Invalid(format!(
            "could not enable WAL mode on {} (got {journal_mode:?}); LANI_HOME must be on a local filesystem",
            path.display()
        )));
    }
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    migrate(&mut conn, path)?;
    Ok(conn)
}

/// The schema version this binary's migrations bring a database to.
fn known_version() -> i64 {
    let count = i64::try_from(MIGRATIONS.len()).expect("migration count fits in i64");
    BASELINE_VERSION + count - 1
}

fn migrate(conn: &mut Connection, path: &Path) -> Result<()> {
    let known = known_version();
    if schema_version(conn)? == known {
        return Ok(());
    }
    // Re-check under a write lock so concurrent first runs apply each migration exactly once.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = schema_version(&tx)?;
    if current > known {
        return Err(Error::Invalid(format!(
            "{} has schema version {current}, but this lani binary only knows versions up to {known}; upgrade lani",
            path.display()
        )));
    }
    if current != 0 && current < BASELINE_VERSION {
        return Err(Error::Invalid(format!(
            "{} was created by a development version of LANI (schema version {current}) whose \
             data cannot be carried over. Stop the server with `lani stop`, move the file (and \
             its -wal and -shm files) aside, and LANI will create a new database.",
            path.display()
        )));
    }
    // Version 0 is a new database: apply everything, starting with the baseline.
    let applied = if current == 0 {
        0
    } else {
        usize::try_from(current - BASELINE_VERSION + 1).expect("schema version is in range")
    };
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(applied) {
        tx.execute_batch(sql)?;
        let version = BASELINE_VERSION + i64::try_from(index).expect("migration count fits in i64");
        tx.pragma_update(None, "user_version", version)?;
    }
    tx.commit()?;
    Ok(())
}

fn schema_version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

/// Refuse to go on once a newer lani has upgraded the schema since this process opened the
/// database: this process's code would misread the new schema, or write rows it no longer fits.
pub fn check_schema(conn: &Connection) -> Result<()> {
    let (current, known) = (schema_version(conn)?, known_version());
    if current > known {
        return Err(Error::Outdated(format!(
            "a newer lani has upgraded the LANI database to schema version {current}, and this lani \
             process only knows versions up to {known}, so it stops rather than misread or damage \
             the data; restart it to run the installed lani"
        )));
    }
    Ok(())
}

/// Begin a write transaction. The schema is checked after the write lock is taken, so no write
/// from this process can reach a schema that a newer lani upgraded meanwhile.
pub fn write_transaction(conn: &mut Connection) -> Result<Transaction<'_>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    check_schema(&tx)?;
    Ok(tx)
}

// ── repos ────────────────────────────────────────────────────────────────────────────────────

/// Return the repo with this git dir, registering it first if it is new.
pub fn upsert_repo(conn: &Connection, git_dir: &str, name: &str, now: &str) -> Result<Repo> {
    conn.execute(
        "INSERT INTO repo (git_dir, name, created_at) VALUES (?1, ?2, ?3)
         ON CONFLICT (git_dir) DO NOTHING",
        params![git_dir, name, now],
    )?;
    find_repo_by_git_dir(conn, git_dir)?
        .ok_or_else(|| Error::NotFound(format!("repo {git_dir} vanished after insert")))
}

pub fn find_repo_by_git_dir(conn: &Connection, git_dir: &str) -> Result<Option<Repo>> {
    Ok(conn
        .query_row(
            "SELECT id, git_dir, name FROM repo WHERE git_dir = ?1",
            [git_dir],
            repo_from_row,
        )
        .optional()?)
}

pub fn get_repo(conn: &Connection, id: i64) -> Result<Repo> {
    conn.query_row(
        "SELECT id, git_dir, name FROM repo WHERE id = ?1",
        [id],
        repo_from_row,
    )
    .optional()?
    .ok_or_else(|| Error::NotFound(format!("repo {id} does not exist")))
}

fn repo_from_row(row: &Row<'_>) -> rusqlite::Result<Repo> {
    Ok(Repo {
        id: row.get(0)?,
        git_dir: row.get(1)?,
        name: row.get(2)?,
    })
}

// ── reviews ──────────────────────────────────────────────────────────────────────────────────

const REVIEW_COLUMNS: &str = "id, repo_id, title, head_ref, base_ref, base_source, status, \
     created_by, created_at, closed_at, summary, summary_updated_by, summary_updated_at, \
     summary_version, turn, turn_by, turn_at, turn_note, rounds";

fn review_from_row(row: &Row<'_>) -> rusqlite::Result<Review> {
    Ok(Review {
        id: row.get(0)?,
        repo_id: row.get(1)?,
        title: row.get(2)?,
        head_ref: row.get(3)?,
        base_ref: row.get(4)?,
        base_source: row.get(5)?,
        status: row.get(6)?,
        created_by: row.get(7)?,
        created_at: row.get(8)?,
        closed_at: row.get(9)?,
        summary: row.get(10)?,
        summary_updated_by: row.get(11)?,
        summary_updated_at: row.get(12)?,
        summary_version: row.get(13)?,
        turn: row.get(14)?,
        turn_by: row.get(15)?,
        turn_at: row.get(16)?,
        turn_note: row.get(17)?,
        rounds: row.get(18)?,
    })
}

pub struct NewReview<'a> {
    pub repo_id: i64,
    pub title: &'a str,
    pub head_ref: &'a str,
    pub base_ref: Option<&'a str>,
    pub base_source: BaseSource,
    pub created_by: Actor,
    pub summary: &'a str,
    pub now: &'a str,
}

/// Insert an open review. Fails with a constraint error if the branch already has one.
pub fn insert_review(conn: &Connection, new: &NewReview<'_>) -> Result<i64> {
    // An empty summary has not been written by anyone yet.
    let summary_updated_by = (!new.summary.is_empty()).then_some(new.created_by);
    let summary_updated_at = (!new.summary.is_empty()).then_some(new.now);
    conn.execute(
        "INSERT INTO review (repo_id, title, head_ref, base_ref, base_source, status, created_by,
                             created_at, closed_at, summary, summary_updated_by,
                             summary_updated_at, summary_version, turn, turn_by, turn_at,
                             turn_note, rounds)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9, ?10, ?11, 1, NULL, NULL, NULL, NULL, 0)",
        params![
            new.repo_id,
            new.title,
            new.head_ref,
            new.base_ref,
            new.base_source,
            ReviewStatus::Open,
            new.created_by,
            new.now,
            new.summary,
            summary_updated_by,
            summary_updated_at,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn get_review(conn: &Connection, id: i64) -> Result<Review> {
    conn.query_row(
        &format!("SELECT {REVIEW_COLUMNS} FROM review WHERE id = ?1"),
        [id],
        review_from_row,
    )
    .optional()?
    .ok_or_else(|| Error::NotFound(format!("review {id} does not exist")))
}

pub fn find_open_review(conn: &Connection, repo_id: i64, head_ref: &str) -> Result<Option<Review>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {REVIEW_COLUMNS} FROM review
                 WHERE repo_id = ?1 AND head_ref = ?2 AND status = 'open'"
            ),
            params![repo_id, head_ref],
            review_from_row,
        )
        .optional()?)
}

/// List reviews, newest first. `None` filters mean "all".
pub fn list_reviews(
    conn: &Connection,
    status: Option<ReviewStatus>,
    repo_id: Option<i64>,
) -> Result<Vec<Review>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {REVIEW_COLUMNS} FROM review
         WHERE (?1 IS NULL OR status = ?1) AND (?2 IS NULL OR repo_id = ?2)
         ORDER BY id DESC"
    ))?;
    let rows = statement.query_map(params![status, repo_id], review_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Replace the summary if it is still at `expected_version`. Returns whether it was updated.
pub fn update_summary(
    conn: &Connection,
    review_id: i64,
    summary: &str,
    actor: Actor,
    now: &str,
    expected_version: i64,
) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE review
         SET summary = ?1, summary_updated_by = ?2, summary_updated_at = ?3,
             summary_version = summary_version + 1
         WHERE id = ?4 AND summary_version = ?5",
        params![summary, actor, now, review_id, expected_version],
    )?;
    Ok(changed == 1)
}

pub fn close_review(conn: &Connection, review_id: i64, now: &str) -> Result<()> {
    conn.execute(
        "UPDATE review SET status = 'closed', closed_at = ?1 WHERE id = ?2",
        params![now, review_id],
    )?;
    Ok(())
}

/// Reopen a closed review. Fails with a constraint error if the branch has another open review.
pub fn reopen_review(conn: &Connection, review_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE review SET status = 'open', closed_at = NULL WHERE id = ?1",
        [review_id],
    )?;
    Ok(())
}

pub fn update_review_title(conn: &Connection, review_id: i64, title: &str) -> Result<()> {
    conn.execute(
        "UPDATE review SET title = ?1 WHERE id = ?2",
        params![title, review_id],
    )?;
    Ok(())
}

pub fn set_review_base(
    conn: &Connection,
    review_id: i64,
    base_ref: Option<&str>,
    base_source: BaseSource,
) -> Result<()> {
    conn.execute(
        "UPDATE review SET base_ref = ?1, base_source = ?2 WHERE id = ?3",
        params![base_ref, base_source, review_id],
    )?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IssueCounts {
    pub open: u64,
    pub resolved: u64,
}

pub fn issue_counts(conn: &Connection, review_id: i64) -> Result<IssueCounts> {
    let mut statement =
        conn.prepare("SELECT status, count(*) FROM issue WHERE review_id = ?1 GROUP BY status")?;
    let mut counts = IssueCounts::default();
    let rows = statement.query_map([review_id], |row| {
        let count: i64 = row.get(1)?;
        let count =
            u64::try_from(count).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(1, count))?;
        Ok((row.get::<_, IssueStatus>(0)?, count))
    })?;
    for row in rows {
        match row? {
            (IssueStatus::Open, count) => counts.open = count,
            (IssueStatus::Resolved, count) => counts.resolved = count,
        }
    }
    Ok(counts)
}

// ── issues ───────────────────────────────────────────────────────────────────────────────────

const ISSUE_COLUMNS: &str = "id, review_id, author, title, severity, status, resolved_by, \
     resolved_at, created_at, edited_by, edited_at, version, asked_by, asked_at";

/// An issue row; its spans are loaded separately.
fn issue_from_row(row: &Row<'_>) -> rusqlite::Result<Issue> {
    Ok(Issue {
        id: row.get(0)?,
        review_id: row.get(1)?,
        author: row.get(2)?,
        title: row.get(3)?,
        severity: row.get(4)?,
        status: row.get(5)?,
        resolved_by: row.get(6)?,
        resolved_at: row.get(7)?,
        anchors: Vec::new(),
        created_at: row.get(8)?,
        edited_by: row.get(9)?,
        edited_at: row.get(10)?,
        version: row.get(11)?,
        asked_by: row.get(12)?,
        asked_at: row.get(13)?,
    })
}

const SPAN_COLUMNS: &str =
    "issue_id, anchor_commit, path, side, start_line, end_line, snippet, snippet_start";

fn span_from_row(row: &Row<'_>) -> rusqlite::Result<(i64, Anchor)> {
    Ok((
        row.get(0)?,
        Anchor {
            commit: row.get(1)?,
            path: row.get(2)?,
            side: row.get(3)?,
            start_line: row.get(4)?,
            end_line: row.get(5)?,
            snippet: row.get(6)?,
            snippet_start: row.get(7)?,
        },
    ))
}

/// A new issue's own fields; its first comment is inserted separately, in the same transaction.
pub struct NewIssue<'a> {
    pub review_id: i64,
    pub author: Actor,
    pub title: &'a str,
    pub severity: Severity,
    /// One or more spans, in the author's order.
    pub anchors: &'a [Anchor],
    pub now: &'a str,
}

/// Insert an open issue and its spans. Its first comment must be inserted in the same
/// transaction.
pub fn insert_issue(conn: &Connection, issue: &NewIssue<'_>) -> Result<i64> {
    conn.execute(
        "INSERT INTO issue (review_id, author, title, severity, status, resolved_by, resolved_at,
                            created_at, edited_by, edited_at, version, asked_by, asked_at)
         VALUES (?1, ?2, ?3, ?4, 'open', NULL, NULL, ?5, NULL, NULL, 1, NULL, NULL)",
        params![
            issue.review_id,
            issue.author,
            issue.title,
            issue.severity,
            issue.now
        ],
    )?;
    let issue_id = conn.last_insert_rowid();
    let mut insert = conn.prepare(
        "INSERT INTO issue_span (issue_id, position, anchor_commit, path, side, start_line,
                                 end_line, snippet, snippet_start)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    for (position, anchor) in (1_i64..).zip(issue.anchors) {
        insert.execute(params![
            issue_id,
            position,
            anchor.commit,
            anchor.path,
            anchor.side,
            anchor.start_line,
            anchor.end_line,
            anchor.snippet,
            anchor.snippet_start,
        ])?;
    }
    Ok(issue_id)
}

/// Change an issue's title and severity if it is still at `expected_version`; returns whether it
/// was.
pub fn update_issue_details(
    conn: &Connection,
    issue_id: i64,
    title: &str,
    severity: Severity,
    actor: Actor,
    now: &str,
    expected_version: i64,
) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE issue
         SET title = ?1, severity = ?2, edited_by = ?3, edited_at = ?4, version = version + 1
         WHERE id = ?5 AND version = ?6",
        params![title, severity, actor, now, issue_id, expected_version],
    )?;
    Ok(changed == 1)
}

pub fn get_issue(conn: &Connection, id: i64) -> Result<Issue> {
    let mut issue = conn
        .query_row(
            &format!("SELECT {ISSUE_COLUMNS} FROM issue WHERE id = ?1"),
            [id],
            issue_from_row,
        )
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("issue {id} does not exist")))?;
    let mut statement = conn.prepare(&format!(
        "SELECT {SPAN_COLUMNS} FROM issue_span WHERE issue_id = ?1 ORDER BY position"
    ))?;
    for span in statement.query_map([id], span_from_row)? {
        issue.anchors.push(span?.1);
    }
    Ok(issue)
}

/// List all of a review's issues with their spans, by issue number, oldest first.
pub fn list_issues(conn: &Connection, review_id: i64) -> Result<Vec<Issue>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {ISSUE_COLUMNS} FROM issue WHERE review_id = ?1 ORDER BY id"
    ))?;
    let mut issues = statement
        .query_map([review_id], issue_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut statement = conn.prepare(
        "SELECT s.issue_id, s.anchor_commit, s.path, s.side, s.start_line, s.end_line,
                s.snippet, s.snippet_start
         FROM issue_span s JOIN issue i ON i.id = s.issue_id
         WHERE i.review_id = ?1 ORDER BY s.issue_id, s.position",
    )?;
    // Both queries are ordered by issue id, so the spans can be handed out in one pass.
    let mut next = issues.iter_mut().peekable();
    for span in statement.query_map([review_id], span_from_row)? {
        let (issue_id, anchor) = span?;
        while next.peek().is_some_and(|issue| issue.id < issue_id) {
            next.next();
        }
        match next.peek_mut() {
            Some(issue) if issue.id == issue_id => issue.anchors.push(anchor),
            _ => {
                return Err(Error::Internal(format!(
                    "span of issue {issue_id} has no issue in review {review_id}"
                )));
            }
        }
    }
    Ok(issues)
}

/// Set an issue's status; `resolved_by` must be `Some` exactly when resolving.
pub fn set_issue_status(
    conn: &Connection,
    issue_id: i64,
    status: IssueStatus,
    resolved_by: Option<Actor>,
    now: &str,
) -> Result<()> {
    let resolved_at = resolved_by.map(|_| now);
    conn.execute(
        "UPDATE issue SET status = ?1, resolved_by = ?2, resolved_at = ?3 WHERE id = ?4",
        params![status, resolved_by, resolved_at, issue_id],
    )?;
    Ok(())
}

// ── comments ─────────────────────────────────────────────────────────────────────────────────

const COMMENT_COLUMNS: &str =
    "id, review_id, issue_id, author, body, head_sha, created_at, edited_by, edited_at, version";

fn comment_from_row(row: &Row<'_>) -> rusqlite::Result<Comment> {
    Ok(Comment {
        id: row.get(0)?,
        review_id: row.get(1)?,
        issue_id: row.get(2)?,
        author: row.get(3)?,
        body: row.get(4)?,
        head_sha: row.get(5)?,
        created_at: row.get(6)?,
        edited_by: row.get(7)?,
        edited_at: row.get(8)?,
        version: row.get(9)?,
    })
}

pub struct NewComment<'a> {
    pub review_id: i64,
    /// `None` for a comment on the review summary.
    pub issue_id: Option<i64>,
    pub author: Actor,
    pub body: &'a str,
    pub head_sha: &'a str,
    pub now: &'a str,
}

pub fn insert_comment(conn: &Connection, new: &NewComment<'_>) -> Result<i64> {
    conn.execute(
        "INSERT INTO comment (review_id, issue_id, author, body, head_sha, created_at,
                              edited_by, edited_at, version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, 1)",
        params![
            new.review_id,
            new.issue_id,
            new.author,
            new.body,
            new.head_sha,
            new.now
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn get_comment(conn: &Connection, id: i64) -> Result<Comment> {
    conn.query_row(
        &format!("SELECT {COMMENT_COLUMNS} FROM comment WHERE id = ?1"),
        [id],
        comment_from_row,
    )
    .optional()?
    .ok_or_else(|| Error::NotFound(format!("comment c{id} does not exist")))
}

/// An issue's comments, oldest first; the first is the issue's description.
pub fn issue_comments(conn: &Connection, issue_id: i64) -> Result<Vec<Comment>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {COMMENT_COLUMNS} FROM comment WHERE issue_id = ?1 ORDER BY id"
    ))?;
    let rows = statement.query_map([issue_id], comment_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Comments on a review's summary, oldest first.
pub fn summary_comments(conn: &Connection, review_id: i64) -> Result<Vec<Comment>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {COMMENT_COLUMNS} FROM comment
         WHERE review_id = ?1 AND issue_id IS NULL ORDER BY id"
    ))?;
    let rows = statement.query_map([review_id], comment_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Replace a comment's body if it is still at `expected_version`. Returns whether it was updated.
pub fn update_comment_body(
    conn: &Connection,
    comment_id: i64,
    body: &str,
    actor: Actor,
    now: &str,
    expected_version: i64,
) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE comment SET body = ?1, edited_by = ?2, edited_at = ?3, version = version + 1
         WHERE id = ?4 AND version = ?5",
        params![body, actor, now, comment_id, expected_version],
    )?;
    Ok(changed == 1)
}

// ── events ───────────────────────────────────────────────────────────────────────────────────

pub struct NewEvent<'a> {
    pub review_id: i64,
    pub issue_id: Option<i64>,
    pub comment_id: Option<i64>,
    pub kind: EventKind,
    pub actor: Actor,
    pub now: &'a str,
}

pub fn insert_event(conn: &Connection, event: &NewEvent<'_>) -> Result<()> {
    conn.execute(
        "INSERT INTO event (review_id, issue_id, comment_id, kind, actor, turn_to, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6)",
        params![
            event.review_id,
            event.issue_id,
            event.comment_id,
            event.kind,
            event.actor,
            event.now
        ],
    )?;
    Ok(())
}

/// The id of the review's latest event; `None` if the review does not exist, since creating a
/// review records its first event.
pub fn latest_event_id(conn: &Connection, review_id: i64) -> Result<Option<i64>> {
    Ok(conn.query_row(
        "SELECT max(id) FROM event WHERE review_id = ?1",
        [review_id],
        |row| row.get(0),
    )?)
}

/// A review's events, newest first: the latest `limit`, or all of them for `None`.
pub fn list_events(conn: &Connection, review_id: i64, limit: Option<u32>) -> Result<Vec<Event>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {EVENT_COLUMNS} FROM event WHERE review_id = ?1 ORDER BY id DESC LIMIT ?2"
    ))?;
    // A negative LIMIT means no limit.
    let limit = limit.map_or(-1, i64::from);
    let rows = statement.query_map(params![review_id, limit], event_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// How many events a review has.
pub fn count_events(conn: &Connection, review_id: i64) -> Result<u64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM event WHERE review_id = ?1",
        [review_id],
        |row| {
            let count: i64 = row.get(0)?;
            u64::try_from(count).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, count))
        },
    )?)
}

/// Every change of a review's turn, with its note, newest first.
pub fn list_hand_offs(conn: &Connection, review_id: i64) -> Result<Vec<Event>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {EVENT_COLUMNS} FROM event WHERE review_id = ?1 AND kind = 'turn_changed'
         ORDER BY id DESC"
    ))?;
    let rows = statement.query_map([review_id], event_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

const EVENT_COLUMNS: &str =
    "id, review_id, issue_id, comment_id, kind, actor, turn_to, note, created_at";

fn event_from_row(row: &Row<'_>) -> rusqlite::Result<Event> {
    Ok(Event {
        id: row.get(0)?,
        review_id: row.get(1)?,
        issue_id: row.get(2)?,
        comment_id: row.get(3)?,
        kind: row.get(4)?,
        actor: row.get(5)?,
        turn_to: row.get(6)?,
        note: row.get(7)?,
        created_at: row.get(8)?,
    })
}

// ── turns and questions for the human (§11.5) ─────────────────────────────────────────────────

/// A change of a review's turn: the new holder (`None` for nobody), who made the change, the note
/// given with it, and the round count afterwards.
pub struct TurnChange<'a> {
    pub review_id: i64,
    pub turn: Option<Actor>,
    pub by: Actor,
    pub note: Option<&'a str>,
    pub rounds: u32,
    pub now: &'a str,
}

/// Record a change of turn on the review and in its activity log.
pub fn change_turn(conn: &Connection, change: &TurnChange<'_>) -> Result<()> {
    conn.execute(
        "UPDATE review SET turn = ?1, turn_by = ?2, turn_at = ?3, turn_note = ?4, rounds = ?5
         WHERE id = ?6",
        params![
            change.turn,
            change.by,
            change.now,
            change.note,
            change.rounds,
            change.review_id
        ],
    )?;
    conn.execute(
        "INSERT INTO event (review_id, issue_id, comment_id, kind, actor, turn_to, note, created_at)
         VALUES (?1, NULL, NULL, 'turn_changed', ?2, ?3, ?4, ?5)",
        params![
            change.review_id,
            change.by,
            change.turn,
            change.note,
            change.now
        ],
    )?;
    Ok(())
}

/// Whether `actor` has handed the review on before.
pub fn has_handed_off(conn: &Connection, review_id: i64, actor: Actor) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM event
                        WHERE review_id = ?1 AND kind = 'turn_changed' AND actor = ?2)",
        params![review_id, actor],
        |row| row.get(0),
    )?)
}

/// Mark an issue as asked about by `role`, or clear the question with `None`.
pub fn set_asked(conn: &Connection, issue_id: i64, role: Option<Actor>, now: &str) -> Result<()> {
    conn.execute(
        "UPDATE issue SET asked_by = ?1, asked_at = ?2 WHERE id = ?3",
        params![role, role.map(|_| now), issue_id],
    )?;
    Ok(())
}

/// An issue with an open question for the human.
#[derive(Clone, Debug)]
pub struct Question {
    pub issue_id: i64,
    pub title: String,
    pub asked_by: Actor,
}

/// The issues of a review with an open question for the human, by issue number.
pub fn questions(conn: &Connection, review_id: i64) -> Result<Vec<Question>> {
    let mut statement = conn.prepare(
        "SELECT id, title, asked_by FROM issue
         WHERE review_id = ?1 AND asked_by IS NOT NULL ORDER BY id",
    )?;
    let rows = statement.query_map([review_id], |row| {
        Ok(Question {
            issue_id: row.get(0)?,
            title: row.get(1)?,
            asked_by: row.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ── settings ─────────────────────────────────────────────────────────────────────────────────

pub fn get_setting(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM setting WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()?)
}

pub fn set_setting(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO setting (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}
