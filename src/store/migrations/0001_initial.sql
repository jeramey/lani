-- LANI schema v1. See docs/design.md §7.3.

CREATE TABLE repo (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  git_dir    TEXT NOT NULL UNIQUE,                  -- canonical absolute path of the common git dir
  name       TEXT NOT NULL,                         -- display name (main worktree basename)
  created_at TEXT NOT NULL
) STRICT;

CREATE TABLE review (
  id                 INTEGER PRIMARY KEY AUTOINCREMENT,
  repo_id            INTEGER NOT NULL REFERENCES repo(id),
  title              TEXT NOT NULL CHECK (length(title) > 0),
  head_ref           TEXT NOT NULL,                 -- full ref name, e.g. refs/heads/feature/x
  base_ref           TEXT,                          -- full ref name; NULL = whole-tree review
  base_source        TEXT NOT NULL CHECK (base_source IN ('auto', 'explicit', 'web_ui')),
  status             TEXT NOT NULL CHECK (status IN ('open', 'closed')),
  created_by         TEXT NOT NULL CHECK (created_by IN ('human', 'reviewer', 'implementer')),
  created_at         TEXT NOT NULL,
  closed_at          TEXT,
  summary            TEXT NOT NULL,                 -- Markdown; '' until someone writes one
  summary_updated_by TEXT CHECK (summary_updated_by IN ('human', 'reviewer', 'implementer')),
  summary_updated_at TEXT,
  summary_version    INTEGER NOT NULL,
  CHECK ((status = 'open') = (closed_at IS NULL)),
  CHECK (base_ref IS NULL OR base_ref <> head_ref),
  CHECK ((summary_updated_by IS NULL) = (summary_updated_at IS NULL))
) STRICT;
CREATE UNIQUE INDEX review_one_open_per_branch ON review(repo_id, head_ref) WHERE status = 'open';

CREATE TABLE issue (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  review_id     INTEGER NOT NULL REFERENCES review(id),
  author        TEXT NOT NULL CHECK (author IN ('human', 'reviewer', 'implementer')),
  status        TEXT NOT NULL CHECK (status IN ('open', 'resolved')),
  resolved_by   TEXT CHECK (resolved_by IN ('human', 'reviewer')),  -- implementers cannot resolve
  resolved_at   TEXT,
  anchor_commit TEXT NOT NULL,                      -- commit whose content the author read
  path          TEXT NOT NULL,
  side          TEXT NOT NULL CHECK (side IN ('head', 'base')),
  start_line    INTEGER NOT NULL CHECK (start_line >= 1),
  end_line      INTEGER NOT NULL CHECK (end_line >= start_line),
  snippet       TEXT NOT NULL,                      -- anchored lines ±3 context lines, at creation
  snippet_start INTEGER NOT NULL,                   -- line number of the snippet's first line
  created_at    TEXT NOT NULL,
  UNIQUE (id, review_id),                           -- parent key for comment's composite FK
  CHECK ((status = 'open'     AND resolved_by IS NULL     AND resolved_at IS NULL)
      OR (status = 'resolved' AND resolved_by IS NOT NULL AND resolved_at IS NOT NULL))
) STRICT;
CREATE INDEX issue_by_review ON issue(review_id, status);

CREATE TABLE comment (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  review_id  INTEGER NOT NULL REFERENCES review(id),
  issue_id   INTEGER,                               -- NULL: a comment on the review summary
  author     TEXT NOT NULL CHECK (author IN ('human', 'reviewer', 'implementer')),
  body       TEXT NOT NULL CHECK (length(body) > 0), -- Markdown
  head_sha   TEXT NOT NULL,                         -- review head commit when the comment was written
  created_at TEXT NOT NULL,
  edited_by  TEXT CHECK (edited_by IN ('human', 'reviewer', 'implementer')),
  edited_at  TEXT,
  version    INTEGER NOT NULL,
  FOREIGN KEY (issue_id, review_id) REFERENCES issue(id, review_id),
  CHECK ((edited_by IS NULL) = (edited_at IS NULL))
) STRICT;
CREATE INDEX comment_by_issue ON comment(issue_id, id);
CREATE INDEX comment_by_review ON comment(review_id, id);

-- Append-only change feed and activity timeline.
CREATE TABLE event (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  review_id  INTEGER NOT NULL REFERENCES review(id),
  issue_id   INTEGER REFERENCES issue(id),
  comment_id INTEGER REFERENCES comment(id),
  kind       TEXT NOT NULL CHECK (kind IN (
               'review_created', 'review_retitled', 'review_base_changed', 'review_closed',
               'review_reopened', 'summary_updated', 'issue_created', 'issue_resolved',
               'issue_reopened', 'comment_added', 'comment_edited')),
  actor      TEXT NOT NULL CHECK (actor IN ('human', 'reviewer', 'implementer')),
  created_at TEXT NOT NULL
) STRICT;
CREATE INDEX event_by_review ON event(review_id, id);

CREATE TABLE setting (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
) STRICT;
INSERT INTO setting (key, value) VALUES ('idle_timeout', '3h');
