# LANI — Local Annotations, No Internet

**Status:** Draft for review, revision 3 (incorporates the answers recorded in §22) · **Date:** 2026-10-07

LANI is a local replacement for the GitHub pull-request review loop. A Claude Code *reviewer* session
files review issues against a git branch, and you read and edit them in a local web UI. A Claude
Code *implementer* session fixes the code and replies, and then the reviewer verifies and resolves.
All of it happens on your machine. You open a GitHub PR only after the loop has converged.

---

## 1. Requirements

| #   | Requirement                                                                                   | Where addressed |
| --- | --------------------------------------------------------------------------------------------- | --------------- |
| R1  | Single executable; macOS and Linux                                                            | §5, §18         |
| R2  | Executes quickly with minimal resources                                                       | §5, §16         |
| R3  | SQLite storage                                                                                | §7              |
| R4  | MCP interface that is easy to configure in Claude Code                                        | §12             |
| R5  | MCP can fetch all issues + comments, or only unresolved ones                                  | §12.3           |
| R6  | Web UI to view and edit reviews, issues and comments                                          | §13             |
| R7  | Each issue has a linear chain of comments, like a GitHub review thread                        | §4, §7          |
| R8  | Web UI source browsing with good syntax highlighting; you can open your own issues            | §13.3, §13.6    |
| R9  | Claude can open the web UI at a given view; reuse an already-running server                   | §12.3, §13.1    |
| R10 | Localhost URL protected by a hard-to-guess token (Jupyter-style)                              | §13.2, §15      |
| R11 | It is always clear whether you or Claude Code wrote an issue or comment; text stays editable  | §11.1, §13.4    |
| R12 | Each review has a title and a summary of intent with its own comment thread; one review per branch | §4, §7     |
| R13 | Every issue is anchored to specific lines and has a one-line title and a severity             | §4, §10         |
| R14 | Only committed code is reviewable                                                             | §10.1           |
| R15 | Only the reviewer and you can resolve issues; the implementer cannot                          | §11.2, §11.3    |
| R16 | Web server stops after 3 h idle by default; tunable (including "never") in the web UI and with a CLI flag | §13.1 |
| R17 | Web UI lists open reviews across all repos                                                    | §13.3           |
| R18 | Diff reviews against an auto-detected parent or chosen base branch; whole-tree reviews with no base; base changeable in the web UI | §9 |
| R19 | Reviewer and implementer sessions take turns automatically; Claude can bring in the human, for the whole review or one issue | §11.5 |

### Non-goals (v1)

- **No network access.** LANI makes no outbound connections. Its only sockets are loopback HTTP
  between its own processes and your browser. All web assets are vendored into the binary.
- **No GitHub integration.** Exporting a converged review to a GitHub PR is deferred (§21).
- **No repository writes.** LANI runs only read-only git commands and never touches your working
  tree, index, refs or config.
- **No uncommitted code.** Reviews cover commits only, as on GitHub.
- **No drafts.** Everything is visible as soon as it is posted. Your control is editing any text
  and resolving issues, not approving posts before they appear.
- **One human per OS user.** There are no accounts.
- **No Windows support.**

---

## 2. The workflow in LANI

```
 Implementer session          LANI (SQLite + git)          Reviewer session            You (browser)
 ───────────────────          ───────────────────          ────────────────            ─────────────
 work on feature/x, commit
 (optional) start_review
   with a summary of intent
                                                           start_review (picks up the
                                                             open review, if any)
                                                           create_issue × N
                                                           open_web_ui(review) ──────► read; edit any issue,
                                                                                       comment or the summary;
                                                                                       open your own issues
 list_issues(status=open) ◄──
 validate, fix, commit
 add_comment(reply) ────────►
                                                           list_issues(status=open)
                                                           verify each reply
                                                           set_issue_status(resolved)
                                                             or add_comment(...)
                                                           create_issue for new findings
                                                                                       review, edit, resolve
 … repeat until no open issues remain; you close the review and open the GitHub PR

 With turns (§11.5), each session ends its pass with hand_off and waits with `lani wait` until the
 review is handed back, so the loop runs by itself until a session hands it to you.
```

Example prompts:

- Implementer (optional, like opening a PR): *"Open a lani review of this branch with a summary of
  what it does and why."*
- Reviewer: *"Start a lani review of this branch."* This compares against the auto-detected parent
  branch (§9) and picks up the review if one is already open.
- Reviewer: *"Start a lani review of all of main."* Run on the default branch, this gives a
  whole-tree review with no base.
- Implementer: *"Pull the open lani issues for this branch, validate them, fix the valid ones,
  commit, and reply to each issue."*
- Reviewer: *"Check the lani replies and resolve the issues that are actually fixed."*
- Either session: *"Open the lani review in my browser"* or *"Show me lani issue 57."*

Both Claude sessions can run in the same checkout. When filing an issue, the reviewer passes the
commit it read and the worktree it read the file from. LANI anchors the issue to that commit and
rejects any anchor that uncommitted changes could have shifted (§10.1). The implementer commits its
fixes so the reviewer can diff them.

---

## 3. Design principles

- **Explicit over implicit.** MCP tools take explicit parameters (`author`, `status`, `base_mode`,
  `commit`, `expected_version`) rather than relying on hidden state. Derived values, such as a
  detected base branch, are always echoed back along with how they were derived. The one piece of
  per-session state, the role lock (§11.3), is stated in the server instructions and in its error
  messages.
- **One domain layer.** The MCP server and the web server call the same service module. All
  business rules live there once: permissions, anchor validation, base detection and edit
  conflicts.
- **Git is the source of truth for code.** LANI stores annotations, not code. The exception is a
  small snippet per issue, so issues stay readable after the code changes or commits are rewritten.
- **Read-only toward repositories.** See Non-goals.
- **Cheap when idle.** MCP processes exist only while a Claude session runs. The web server does no
  work without a connected browser and stops itself after the idle timeout.

---

## 4. Concepts

| GitHub                          | LANI                   | Notes |
| ------------------------------- | ---------------------- | ----- |
| Repository                      | **Repo**               | Identified by its *common git dir*, so all worktrees of a repo are one Repo. |
| Pull request                    | **Review**             | A head branch under review, with a title. A *diff review* compares it to a base branch; a *whole-tree review* has no base (§9). |
| PR description                  | **Review summary**     | Markdown describing the intent of the change, editable by everyone. |
| PR conversation                 | **Summary comments**   | A linear comment thread attached to the summary. |
| Review thread / line comment    | **Issue**              | Anchored to one or more line ranges (*spans*), in one file or several. It has a one-line title, a severity (`high`, `medium`, `low` or `nit`), an `open`/`resolved` status, and a linear chain of comments. |
| Comment / reply                 | **Comment**            | Markdown. An issue's first comment is its description. |
| Resolve conversation            | **Resolve**            | Done by the reviewer or you, never the implementer. |
| Outdated comment                | **Outdated** anchor    | The anchored lines changed after the issue was created. |

**Authors.** Every review, issue, comment and summary edit records who made it: `human`,
`reviewer` or `implementer`. The MCP interface accepts only `reviewer` or `implementer`, so Claude
can never act as you. The web UI always acts as `human`. Authorship never changes after creation,
and edits are marked (§11.1).

**Review identity.** A review belongs to one Repo and one head branch. A branch has at most one
open review. `start_review` on a branch that already has an open review returns that review. If the
branch's earlier reviews are all closed, `start_review` creates a new review, and the closed ones
remain as history. Closed reviews are read-only until you reopen them in the web UI. Reopening fails
if the branch already has another open review.

**Issue titles and severity.** Every issue has a title, one line of at most 200 characters naming
the problem, and a severity: `high`, `medium`, `low`, or `nit` for a minor polish point. Lists show
an issue by its title, severity and location. The author sets both when opening the issue, and only
you can change them later, in the web UI (§11.2). The severity is its own field, so it does not
belong in the title.

**IDs.** Reviews, issues and comments have global integer IDs, shown as `Review 3`, `Issue 57` and
`c201`. A single integer addresses anything, which keeps tool parameters simple.

---

## 5. Implementation language: Rust

**Decision:** Rust (stable, edition 2024), built as one binary named `lani`.

Rationale:

- The toolchain is already installed and current on the development machine (rustc 1.96 with
  clippy, rustfmt and llvm-cov).
- It has the lowest resource footprint of the candidates: no GC or runtime, millisecond startup,
  and small resident memory.
- Mature crates exist for every component (§17).
- Strict compile-time checking covers templates too (askama), and clippy pedantic lints are
  available.

Alternatives considered:

| Option          | Why not chosen |
| --------------- | -------------- |
| Go              | Close second. Pure-Go SQLite makes cross-compiling trivial and builds are faster. It isn't installed, is slightly heavier at runtime, and familiarity favours Rust. |
| TypeScript/Bun  | Shiki highlighting is best-in-class, but single-file executables are ~60–100 MB and use more memory. |
| Python          | No clean single-binary distribution story. |

Known costs:

- Builds are slower than Go: an estimated 1–3 minutes for a clean release build and a few seconds
  for an incremental debug build.
- The bundled SQLite and the Oniguruma regex engine compile C code. A C compiler is present (Apple
  clang 21). Release binaries for other targets need per-OS CI runners or `cargo-zigbuild` (§18).

---

## 6. Architecture

```
  Claude Code (reviewer)    Claude Code (implementer)          Browser
          │ stdio MCP                 │ stdio MCP                 │ HTTP · 127.0.0.1 · token
          ▼                           ▼                           ▼
   ┌─────────────┐             ┌─────────────┐            ┌────────────────┐
   │  lani mcp   │             │  lani mcp   │            │   lani serve   │  detached, one per OS user
   └──────┬──────┘             └──────┬──────┘            └───────┬────────┘
          │                           │                           │
          └─────────────┬─────────────┴───────────────────────────┤
                        ▼                                         ▼
        $LANI_HOME/lani.db  (SQLite, WAL)              git CLI, read-only ──► your repos
```

- **`lani mcp`** is spawned by Claude Code, one per session. It speaks MCP over stdio, opens the
  DB, and exits on stdin EOF.
- **`lani serve`** is the web server, started on demand by `lani open` or the `open_web_ui` tool.
  It detaches so it outlives the Claude session that started it, and stops itself after the idle
  timeout (§13.1). There is one per OS user.
- All processes share one SQLite database in WAL mode. The web server learns about MCP writes
  through an append-only `event` table (§13.5).

---

## 7. Storage

### 7.1 Location

```
$LANI_HOME/                      0700   default: $XDG_DATA_HOME/lani, else ~/.local/share/lani
├── lani.db (+ -wal, -shm)       0600
├── run/                         0700
│   ├── server.json              0600   {pid, port, token, instance_id, version, started_at}
│   ├── server.lock                     held (flock) by the running server
│   └── open-<random>.html       0600   transient browser-launch redirect files (§13.2)
└── logs/server.log                     detached server's stderr, truncated at each start
```

- The XDG path is used on macOS too, so both platforms behave the same. `$LANI_HOME` overrides it.
- `$LANI_HOME` must be on a local filesystem, because SQLite WAL mode does not work over network
  filesystems.
- There is one global database for all repos.

### 7.2 Connection settings

Every connection sets `journal_mode=WAL` (persistent), `synchronous=NORMAL`, `foreign_keys=ON` and
`busy_timeout=5000`. Write transactions use `BEGIN IMMEDIATE`, stay short, and never run git
commands. Expected contention is a few writes per minute.

### 7.3 Schema (v1)

The service layer writes every column explicitly. There are no column defaults.

```sql
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
  base_ref           TEXT,                          -- full ref name; NULL = whole-tree review (§9)
  base_source        TEXT NOT NULL CHECK (base_source IN ('auto', 'explicit', 'web_ui')),
  status             TEXT NOT NULL CHECK (status IN ('open', 'closed')),
  created_by         TEXT NOT NULL CHECK (created_by IN ('human', 'reviewer', 'implementer')),
  created_at         TEXT NOT NULL,
  closed_at          TEXT,
  summary            TEXT NOT NULL,                 -- Markdown; '' until someone writes one
  summary_updated_by TEXT CHECK (summary_updated_by IN ('human', 'reviewer', 'implementer')),
  summary_updated_at TEXT,
  summary_version    INTEGER NOT NULL,              -- optimistic concurrency (§11.4)
  -- Added by migration 3 (§11.5):
  turn               TEXT CHECK (turn IN ('reviewer', 'implementer', 'human')),  -- NULL: nobody
  turn_by            TEXT CHECK (turn_by IN ('human', 'reviewer', 'implementer')),
  turn_at            TEXT CHECK ((turn_by IS NULL) = (turn_at IS NULL)),
  turn_note          TEXT,                          -- Markdown, given with the last change of turn
  rounds             INTEGER NOT NULL DEFAULT 0 CHECK (rounds >= 0),  -- since the human handed it on
  CHECK ((status = 'open') = (closed_at IS NULL)),
  CHECK (base_ref IS NULL OR base_ref <> head_ref),
  CHECK ((summary_updated_by IS NULL) = (summary_updated_at IS NULL))
) STRICT;
CREATE UNIQUE INDEX review_one_open_per_branch ON review(repo_id, head_ref) WHERE status = 'open';

CREATE TABLE issue (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  review_id     INTEGER NOT NULL REFERENCES review(id),
  author        TEXT NOT NULL CHECK (author IN ('human', 'reviewer', 'implementer')),
  title         TEXT NOT NULL CHECK (length(title) BETWEEN 1 AND 200),  -- one line
  severity      TEXT NOT NULL CHECK (severity IN ('high', 'medium', 'low', 'nit')),
  status        TEXT NOT NULL CHECK (status IN ('open', 'resolved')),
  resolved_by   TEXT CHECK (resolved_by IN ('human', 'reviewer')),  -- implementers cannot resolve
  resolved_at   TEXT,
  created_at    TEXT NOT NULL,
  edited_by     TEXT CHECK (edited_by IN ('human', 'reviewer', 'implementer')),  -- title/severity
  edited_at     TEXT,
  version       INTEGER NOT NULL,                   -- of title and severity (§11.4)
  -- Added by migration 3 (§11.5): an open question for the human.
  asked_by      TEXT CHECK (asked_by IN ('reviewer', 'implementer')),
  asked_at      TEXT CHECK ((asked_by IS NULL) = (asked_at IS NULL)),
  UNIQUE (id, review_id),                           -- parent key for comment's composite FK
  CHECK ((status = 'open'     AND resolved_by IS NULL     AND resolved_at IS NULL)
      OR (status = 'resolved' AND resolved_by IS NOT NULL AND resolved_at IS NOT NULL)),
  CHECK ((edited_by IS NULL) = (edited_at IS NULL))
) STRICT;
CREATE INDEX issue_by_review ON issue(review_id, status);

-- The line ranges an issue is about (§10): one or more, inserted with the issue.
CREATE TABLE issue_span (
  issue_id      INTEGER NOT NULL REFERENCES issue(id),
  position      INTEGER NOT NULL CHECK (position BETWEEN 1 AND 20),  -- the author's order
  anchor_commit TEXT NOT NULL,                      -- commit the line numbers refer to (§10)
  path          TEXT NOT NULL,
  side          TEXT NOT NULL CHECK (side IN ('head', 'base')),
  start_line    INTEGER NOT NULL CHECK (start_line >= 1),
  end_line      INTEGER NOT NULL CHECK (end_line >= start_line),
  snippet       TEXT NOT NULL,                      -- the lines ±3 context lines, at creation
  snippet_start INTEGER NOT NULL,                   -- line number of the snippet's first line
  PRIMARY KEY (issue_id, position)
) STRICT, WITHOUT ROWID;

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
  version    INTEGER NOT NULL,                      -- optimistic concurrency (§11.4)
  FOREIGN KEY (issue_id, review_id) REFERENCES issue(id, review_id),
  CHECK ((edited_by IS NULL) = (edited_at IS NULL))
) STRICT;
CREATE INDEX comment_by_issue ON comment(issue_id, id);
CREATE INDEX comment_by_review ON comment(review_id, id);

-- Append-only change feed (§13.5) and activity timeline.
CREATE TABLE event (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  review_id  INTEGER NOT NULL REFERENCES review(id),
  issue_id   INTEGER REFERENCES issue(id),
  comment_id INTEGER REFERENCES comment(id),
  kind       TEXT NOT NULL CHECK (kind IN (
               'review_created', 'review_retitled', 'review_base_changed', 'review_closed',
               'review_reopened', 'summary_updated', 'issue_created', 'issue_edited',
               'issue_resolved', 'issue_reopened', 'comment_added', 'comment_edited',
               'turn_changed', 'human_asked')),
  actor      TEXT NOT NULL CHECK (actor IN ('human', 'reviewer', 'implementer')),
  turn_to    TEXT CHECK (turn_to IN ('reviewer', 'implementer', 'human')),  -- NULL: nobody
  created_at TEXT NOT NULL,
  CHECK (kind = 'turn_changed' OR turn_to IS NULL)
) STRICT;
CREATE INDEX event_by_review ON event(review_id, id);

CREATE TABLE setting (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
) STRICT;
-- Seeded: ('idle_timeout', '3h'), a duration or 'never' (§13.1); and by migration 3
-- ('round_limit', '5'), a whole number from 1 to 50 (§11.5).
```

Notes:

- **Timestamps** are RFC 3339 UTC strings with millisecond precision, e.g.
  `2026-10-07T14:02:11.482Z`. They sort correctly and are readable in the `sqlite3` CLI.
- **Spans.** An issue and its spans are inserted in one transaction, and every issue has at least
  one span; the service layer enforces both, because a CHECK cannot count rows in another table.
- **Root comment.** An issue's root comment is the comment with the lowest `id` for that issue. An
  issue and its root comment are inserted in one transaction.
- **Summary comments.** Comments with `issue_id IS NULL` belong to the review summary. The
  composite foreign key guarantees that an issue comment's `review_id` matches its issue.
- **Deletion.** Nothing is deleted in v1 (§21), and `AUTOINCREMENT` would prevent ID reuse if
  deletion were ever added.
- **Base-side issues.** "Side `base` only in diff reviews" spans two tables, so the service layer
  enforces it (§10).

### 7.4 Migrations and version skew

Migrations are numbered SQL files embedded in the binary and tracked with `PRAGMA user_version`.
They are applied on open inside `BEGIN IMMEDIATE`, so concurrent first runs are safe. Migrations
only move forward.

The schema starts from a baseline at version 2 (`0002_baseline.sql`). Version 1 was the schema of
development builds before issues gained titles and severity; it was restructured rather than
migrated, because no data needed keeping. A database at version 1 is refused with instructions to
stop the server and move the file aside, after which LANI creates a new one.

Migration 3 (`0003_turns.sql`) adds turns and questions for the human (§11.5). Columns added to
existing tables need a default or NULL for the rows already there, which is the one exception to
"no column defaults"; the service still writes every column. The event table gains new kinds,
and SQLite cannot alter a CHECK constraint in place, so the migration rebuilds that table, keeping
every row and id. A binary that finds `user_version` higher than it knows refuses to run and asks
you to upgrade. For example, an old web server still running after a new binary migrated the DB is
fixed with `lani stop && lani open`.

---

## 8. Git integration

LANI shells out to the `git` CLI rather than using a git library, so worktrees, partial clones,
SHA-256 repos and `safe.directory` all behave exactly as you already experience them. **Minimum
git version: 2.41**, the release that added the `%(ahead-behind:…)` format atom used for parent
detection (§9.4). The installed version is 2.54. Some LTS distributions still ship older git; for
example, Debian 12 ships 2.39. Supporting those would need a slower per-branch fallback. LANI
runs `git version` before its first git command, and a missing or older git fails with a message
such as "LANI needs git 2.41 or newer; found git 2.39.2 at /usr/bin/git". Only a passing check is
remembered for the life of the process. After a failure, the next git command checks again, so a
long-running `lani serve` or `lani mcp` picks up a git that was installed or upgraded meanwhile.

Commands used (all read-only):

| Purpose                     | Command (abridged) |
| --------------------------- | ------------------ |
| Identify repo               | `git -C <path> rev-parse --path-format=absolute --git-common-dir --show-toplevel` |
| Current branch of a path    | `git -C <path> symbolic-ref -q HEAD` |
| Resolve ref → commit        | `git rev-parse --verify --end-of-options <ref>^{commit}` |
| Default branch              | `git symbolic-ref -q refs/remotes/origin/HEAD`; `git config init.defaultBranch` |
| Parent candidates           | `git for-each-ref --format='%(refname)%00%(ahead-behind:<head>)' refs/heads/` |
| Branch list (base picker)   | `git for-each-ref --format='%(refname)' refs/heads/ refs/remotes/` |
| Merge base                  | `git merge-base <base> <head>` |
| Commit is on the branch     | `git merge-base --is-ancestor <commit> <head>` |
| Renames for relocation      | `git diff --name-status -z -M <anchor_commit> <head>` |
| Changed files (code views)  | `git diff --raw -z -M --no-abbrev <merge-base> <head>` |
| Unified diff                | `git diff -M -U3 <merge-base> <head> -- <old path> <new path>`, one call per file |
| Line relocation             | `git diff -M -U0 <anchor_commit> <head> -- <old path> <new path>` |
| File on disk vs commit      | `git -C <worktree> diff-index -p -U0 <commit> -- <path>` |
| Worktree state              | `git -C <worktree> status --porcelain=v2 -z --untracked-files=no` |
| Directory listing           | `git ls-tree -z --long --full-tree <commit> -- <dir>/` |
| One file or directory       | `git ls-tree -z --long --full-tree <commit> -- <path>` |
| File content                | `git cat-file blob <blob-sha>` |
| Blobs of a page's diffs     | `git cat-file --batch-check`, then `--batch`, one call each per page |

Rules for every invocation:

- **No worktree dependence.** Repo reads use `git --git-dir=<common git dir>`. The exceptions are
  the worktree checks (§10.1, `start_review`, `get_review`), which run with `-C <worktree>`.
- **No index writes.** Every call passes `--no-optional-locks`, which stops `git status` from
  refreshing and rewriting a worktree's index. Porcelain `git diff` against the working tree
  rewrites the index even with that flag (verified with git 2.54), so comparisons with files on
  disk use plumbing `git diff-index`, which never writes it. A test checks that the index is
  byte-identical afterwards.
- **No option injection.** User-supplied refs are passed only to `rev-parse`, after
  `--end-of-options`; every other command receives the full SHAs `rev-parse` returned. Paths
  always follow `--`, and `GIT_LITERAL_PATHSPECS=1` stops them being read as glob patterns.
- **No redirection.** Environment variables that would point git at another repository or index
  (`GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE` and similar) are removed from every invocation.
- **Stable output.** These overrides make output independent of user config: `-c
  core.quotePath=false`, `-c diff.suppressBlankEmpty=false` (otherwise a blank context line has
  no leading space), `--no-color --no-ext-diff --no-textconv`, and `LC_ALL=C`. From unified
  diffs LANI reads only `@@` hunk headers and the first character of each hunk line, never file
  headers. Paths come from `-z` name-status or raw output, and line text from the blobs. That is
  why the diff view runs one `git diff` per file: splitting a multi-file diff would mean matching
  its file headers to paths, and a type change shows up as two sections.
- **Exact hunks.** Every diff whose hunks LANI parses also passes `--text`, so files git treats
  as binary (`-diff` in `.gitattributes`, or a NUL byte near the start) still produce hunks
  instead of none, which would read as "unchanged". It also passes `--inter-hunk-context=0`, so
  a user's `diff.interHunkContext` cannot merge nearby hunks. A binary-diff marker in the output
  is an error, never an empty diff.
- **Working directory `/`.** Git commands, the detached server and the browser opener run in
  `/`. Commands with `--git-dir` treat the working directory as the work tree, so they would fail
  once the starting session's directory was deleted, and they would read its `.gitattributes`.
- **No git inside transactions.** No git command runs inside a database transaction.

---

## 9. Review types and bases

### 9.1 Two review types

- **Diff review** (`base_ref` set) covers the changes from `merge-base(base, head)` to the head,
  like a PR. The web UI has both a Changes tab and a Files tab.
- **Whole-tree review** (`base_ref` NULL) covers every file at the head commit, for example a total
  review of `main`. The web UI has only the Files tab. Issues anchor to side `head` only.

In both types the head is a branch, and the review follows it as commits are added. Only committed
code is ever reviewed.

### 9.2 Choosing the base when a review starts

`start_review` takes a required `base_mode`:

| `base_mode` | Result |
| ----------- | ------ |
| `ref`       | A diff review against `base_ref`, which can be a local branch, a remote-tracking branch (e.g. `origin/main`), or any ref that resolves to a commit. It must differ from the head. Names that depend on which worktree resolves them are refused (see below). |
| `none`      | A whole-tree review. |
| `auto`      | A whole-tree review if the head is the repo's default branch (§9.3). Otherwise, a diff review against the nearest parent branch (§9.4). |

LANI resolves names in the common git dir, which every worktree shares, so `base_ref` and
`head_ref` must mean the same thing in every worktree. Some names depend on which worktree
resolves them, and in the common git dir they mean the main worktree's:

- `HEAD` and `@` mean that worktree's checked-out branch. `HEAD` is matched in any case,
  because on a case-insensitive filesystem (the macOS default) `head` opens the common git dir's
  `HEAD` file, even from a linked worktree.
- `@{…}` forms such as `@{-1}`, `@{u}` and `main@{1}` read a reflog or an upstream.
- `refs/bisect/`, `refs/worktree/` and `refs/rewritten/` hold refs that each worktree keeps its own
  copy of. This rule applies to `base_ref` only; `head_ref` must be a local branch anyway.

From a linked worktree, any of these could silently pick the wrong branch, so they are refused,
and the error asks for a branch name. No real ref name is `@` or contains `@{`. A branch named
`head`, in any case, is refused under its short name but can be given as `refs/heads/head`.
`head_ref` can simply be left out, since it then defaults to the branch checked out at
`repo_path`, which is read in that worktree.

The response states the resolved base and how it was chosen. For `auto`, it also lists the
runner-up candidates. The review stores `base_source` (`auto`, `explicit` or `web_ui`), so the web
UI can label an auto-detected base and you know to check it.

### 9.3 Default branch

Git has no single notion of a default branch. LANI uses the first of these that matches:

1. The target of `refs/remotes/origin/HEAD` (set by `git clone`, updated by
   `git remote set-head origin --auto`), if a local branch of the same name exists.
2. The `init.defaultBranch` config value, if a local branch of that name exists.
3. A local `main` branch, then `master`.

If none matches, the repo has no default branch, and `auto` always looks for a parent.

### 9.4 Nearest parent branch

Git does not record which branch another branch was created from, so LANI infers it. The parent is
the local branch whose fork point is closest to the head.

1. One command computes, for every local branch C, how many head commits C lacks:
   `git for-each-ref --format='%(refname)%00%(ahead-behind:<head>)' refs/heads/`. The "behind"
   value is the number of commits on the head that are not reachable from C.
2. LANI skips the head branch itself, and every branch whose behind value is 0. Those branches
   already contain all of the head, so they are descendants or sit at the same commit.
3. The branch with the smallest behind value wins.
   - If several branches tie and the default branch is one of them, the default branch wins.
   - Any other tie is rejected with the tied candidates listed. Claude then asks you or passes
     `base_mode: ref`.
4. If no candidates remain, the request is rejected. The error suggests `base_mode: none` for a
   whole-tree review, or `base_mode: ref`.

Examples:

- `feature/b` was branched from `feature/a`, which was branched from `main`. `feature/a` lacks only
  `feature/b`'s commits, so `feature/a` is chosen.
- After `feature/a` has been merged into `main`, the two tie, and `main` is chosen.

Remote-tracking branches are not candidates. A stale local `main` still marks the fork point
correctly. If you want `origin/main`, choose it explicitly or in the web UI.

### 9.5 Changing the base (web UI)

The review overview has a base picker listing local branches, remote-tracking branches, and
**None — review the whole tree**. Saving sets `base_source = 'web_ui'` and records a
`review_base_changed` event. Nothing else needs to change:

- Issues are anchored to commits, so they stay valid.
- The Changes view and relocation are recomputed against the new merge-base.
- If the review no longer has a base, issues on the `base` side are shown with their stored
  snippet.

The base can be changed only in the web UI. `get_review` shows Claude the current base and its
source.

---

## 10. Anchors

Every issue is anchored to 1 to 20 **spans**. A span is an inclusive line range
`start_line..=end_line` in one file, with a `side`:

- `head` is the file in the head commit. This is the normal case.
- `base` is the merge-base version. It is used only for lines a diff review's branch deleted.

Most issues have one span. Several spans cover one problem in several places, such as the same bug
in four templates, in one file or across files. The author lists the main place first; spans keep
that order. They must not overlap one another on the same file and side.

Line numbers only mean something relative to specific file content. So each span's
`anchor_commit` is **the commit whose content the author actually read**:

- For an MCP issue, it is the `commit` Claude passes to `create_issue`.
- For a web UI issue, it is the commit the page was rendered at.
- For side `base`, it is that commit's merge-base with the base branch.

An issue can be on any line of any file at that commit. Unlike GitHub, issues are not limited to
lines inside the diff.

### 10.1 Creating and validating (MCP)

`create_issue` takes a list of `spans`, each with `path`, `side`, `start_line`, `end_line` and
`start_line_text`, and two parameters that say where Claude's view of the files came from:

- **`commit`**: the SHA of the commit whose files Claude read, as `git rev-parse HEAD` prints it
  in its worktree. It should equal the review head that `start_review` and `get_review` report.
  Only a full or abbreviated SHA (4–64 hex digits) is accepted. LANI resolves names in the common
  git dir, where `HEAD`, `@{-1}` and other per-worktree names mean the main worktree's; from a
  linked worktree they would name a commit Claude never read.
- **`worktree_path`**: the worktree Claude read the files from. It is `null` if Claude read them
  from git with `git show <commit>:<path>`. It must be `null` if any span is on side `base`, whose
  content only exists in git.

First, each span's path must be clean and its range must start at line 1 or later. Spans must not
overlap on the same file and side, and there must be 1 to 20 of them. Then validation runs in
this order, and the first failure is reported. Step 1 runs once; steps 2 to 4 run for each span in
turn. When an issue has several spans, a span's error starts with its number and range, such as
`span 2 ("src/a.rs":8-8): …`.


1. **The commit is on the branch.** `commit` must be the review head or an ancestor of it. If not,
   Claude is reviewing the wrong branch, or the branch was rebased after Claude read the code. The
   error says which and gives the current head.
2. **The path and range exist.**
   - Side `base` requires a diff review.
   - `path` must exist in `commit`, or in its merge-base for side `base`.
   - Lines must satisfy `1 ≤ start_line ≤ end_line ≤ line count`.
3. **The file on disk matches the commit.** This applies to side `head` when `worktree_path` is
   given. The worktree must belong to the review's repo.
   - LANI diffs the file on disk against the commit with
     `git -C <worktree> diff-index -p -U0 <commit> -- <path>`. That one diff catches uncommitted
     edits, staged or not, and a `HEAD` that moved after `commit`.
   - LANI then walks the resulting hunks as in §10.2:
     - **No differences:** accepted.
     - **Differences that leave the region's line numbers unchanged:** accepted. This covers
       changes after `end_line` and same-size edits above the region; either way the region has
       the same numbers and content in both versions.
     - **A difference overlapping the region:** rejected. The marked code is not what was
       committed, and it must be committed before it can be reviewed.
     - **Differences above the region that shift its line numbers:** rejected. LANI cannot tell
       which version the line numbers came from. The error gives the region's line numbers in
       both versions so Claude can resubmit with explicit values (§12.4 has an example).
     - **File missing on disk:** rejected.
   - When a difference leads to rejection, LANI checks which commit the worktree has checked
     out. If it is not `commit`, the differences may be committed rather than uncommitted, and
     the likelier mistake is the `commit` passed. The error then names the checked-out commit and
     says to pass its SHA if Claude read the file on disk, or `worktree_path: null` if the line
     numbers came from `commit`.
4. **The first line matches.** `start_line_text` must equal line `start_line` in the anchor commit
   after trimming whitespace.
   - This catches off-by-N line numbers, which LLMs occasionally produce.
   - On mismatch, the error shows the actual line and lists the lines whose content matches.
   - It runs after step 3, so a dirty file is reported as dirty rather than as a mismatch.

LANI never auto-corrects an anchor. Every rejection says how to retry with explicit values.

Each span stores its `anchor_commit` and a snippet: its lines plus 3 context lines on each side,
capped at 200 lines. The root comment's `head_sha` records the review head at filing time. If
that head is already newer than `commit`, because the implementer committed in the meantime, the
issue is relocated straight away (§10.2). The response says where the issue now points, or that it
is outdated.

When given a worktree path, `start_review` and `get_review` report that worktree's state: its
`HEAD`, whether that equals the review head, and its modified tracked files. This way Claude learns
about a dirty tree before it files anything.

The web UI reads files from git, so its new-issue form only needs steps 1 and 2.

### 10.2 Relocation and "outdated"

Each span is relocated on its own. Suppose a span was anchored at commit A and is displayed
against the current head commit H, or against the current merge-base for side `base`.

1. If A = H, the anchor applies as-is.
2. Otherwise, LANI runs `git diff -M -U0 A H` restricted to the anchored path, following renames
   found by `--name-status -M`. It walks the hunks in order, keeping a running line offset:
   - A hunk entirely before `start_line` shifts the range by (added − removed) lines.
   - A hunk that overlaps the range, or inserts lines inside it, makes the issue **outdated**.
   - A hunk after `end_line` ends the walk.
3. If the file was deleted, the issue is **outdated (file removed)**.

A current span is shown at its relocated position. An outdated span is shown with its stored
snippet and original line numbers, as GitHub shows outdated comments. An issue whose spans are
all outdated is *outdated*; one with only some outdated is *partly outdated*. "Outdated" is also a useful
signal to the reviewer that the implementer touched those lines.

Placement work is shared within one request, meaning one tool call or one page. There is one
whole-tree rename diff per (A, H) pair, and each anchor commit's existence and each file's contents
are looked up once. Nothing is cached across requests, so every tool call and page view places
each issue again (§21).

Rebases and force-pushes are fine. Display never needs A to still exist; if A is gone from the
object database, the issue is treated as outdated.

---

## 11. Authorship, editing and permissions

### 11.1 Authorship and edits (R11)

- **Everything is attributed.** Every issue, comment and summary carries its author. In the web UI
  this shows as **You** or **Claude · reviewer** / **Claude · implementer**, with a distinct badge
  and accent colour, so the originator is obvious at a glance.
- **Authorship never changes.** Editing someone else's text records `edited_by` and `edited_at`:
  - The web UI shows *edited by you* on a Claude comment you changed.
  - MCP output shows *edited by human*, so Claude knows the text is no longer purely its own.
  - The summary shows who last edited it.
- **Saving unchanged text is not an edit.** Line endings are stored as LF, so a browser's CRLF
  doesn't count as a change, and a save that leaves the text identical records nothing. The web
  editors show text exactly as stored, including a leading newline, which HTML parsers would
  otherwise drop after a `<textarea>` tag.
- **No drafts.** Everything is visible to all parties as soon as it is posted. You shape the review
  by editing any text, adding issues and comments, and resolving issues.

### 11.2 Permissions

| Action                                    | You (web UI) | reviewer (MCP)     | implementer (MCP)  |
| ----------------------------------------- | ------------ | ------------------ | ------------------ |
| Start a review                            | — (§21)      | ✓                  | ✓                  |
| Change the review title or base           | ✓            | —                  | —                  |
| Edit the review summary                   | ✓            | ✓                  | ✓                  |
| Comment on the summary or an issue        | ✓            | ✓                  | ✓                  |
| Create an issue                           | ✓            | ✓                  | ✓                  |
| Edit a comment                            | any          | own role's only    | own role's only    |
| Change an issue's title or severity       | ✓            | —                  | —                  |
| Resolve or reopen an issue                | ✓            | ✓                  | **✗**              |
| Close a review                            | ✓            | only when you ask  | ✗                  |
| Hand a review on (§11.5)                  | to a role or nobody | on its turn, to the implementer or you | on its turn, to the reviewer or you |
| Ask the human about an issue              | —            | ✓                  | ✓                  |
| Reopen a closed review                    | ✓            | —                  | —                  |

The service layer enforces these rules. For resolution, the database enforces them too: the
`resolved_by` CHECK constraint excludes `implementer`.

### 11.3 Session roles

The MCP `author` parameter is self-declared. To stop an implementer session from slipping into the
reviewer role:

- **Role lock.** A `lani mcp` process serves exactly one Claude Code session. It locks its role at
  the first call that passes `author`. Later calls with a different `author` are rejected, for
  example: *"This session is acting as implementer. Resolving issues is reserved for the reviewer
  and the human."*
- **Instructions.** The server instructions and the `set_issue_status` description say the
  implementer cannot resolve issues.

The limit: a session could still claim `reviewer` on its very first call. Separate MCP
configurations per role would close that gap, but they make setup harder, and the lock is enough
(D11).

### 11.4 Concurrent edits

Comment bodies and the summary each carry a `version`. Every save states the version it was based
on. A stale save is rejected rather than silently overwriting someone else's change:

- In the web UI, the server returns 409, and the form shows the newer text next to yours.
- Over MCP, the tool returns an error that includes the current text and version. The text is
  quoted line by line with the `│ ` prefix, like comments in tool output (§12.4), so it cannot pass
  for LANI's own lines or a message from the human.

---

### 11.5 Turns and questions for the human

A review records whose turn it is: `reviewer`, `implementer`, `human`, or nobody. With it, the two
Claude sessions can run the review loop on their own and bring you in when it matters.

- **Handing on.** A session that finishes its pass calls `hand_off` with who should act next and
  an optional note. The reviewer hands to the implementer after filing or answering issues; the
  implementer hands back after fixing and replying. Turns change only by an explicit hand-off,
  never as a side effect of a comment, because a session is often mid-pass.
  - The reviewer and the implementer hand on a review only when it is their turn or nobody's, and
    only to the other role or to you.
  - You hand it to either role, or to nobody, from the review page. Handing it to a role starts
    the round count again.
  - A closed review takes no hand-offs.
- **Waiting.** After a hand-off, a session runs `lani wait --review N --for <its role>` as a
  background command. It checks the review once a second and exits when it is that role's turn,
  printing who handed it over and their note. It also exits when the review is closed, or after
  an optional `--timeout`. In Claude Code a background command wakes the session when it exits,
  so waiting costs no tokens and the session stays responsive to you.
- **The round limit.** A round is the reviewer handing the review to the implementer and the
  implementer handing it back; it ends with that hand-back. A hand-off from the implementer that
  opens the loop, or that answers your hand-off, starts no round. When the rounds since you last handed the review on reach the limit (default 5, set on the
  Settings page), that hand-off goes to you instead of the reviewer. The reply tells the session
  why. This stops two sessions from ping-ponging indefinitely over a disagreement.
- **Bringing you in.** A session hands the review to you when every issue is resolved, or when it
  needs your decision. The tool's reply tells it to tell you at once: in its reply, with a push
  notification where available, and by opening the review in your browser. It then waits for you
  to hand the review back.
- **Questions about one issue.** A session can ask you about a single issue while the loop carries
  on, with `add_comment` and `ask_human: true`; the comment is the question. The issue shows as
  needing you until you comment on it, resolve it or reopen it, any of which clears the question.
- **Where you see it.** These places show whose turn it is and the questions waiting for you:
  - the review list marks reviews that need you;
  - every review page shows the turn, with the note, and lists the questions as links to their
    issues;
  - issue rows, issue pages and inline threads mark issues that need you, and the overview lists
    them first.

## 12. MCP interface

### 12.1 Setup

```sh
cargo install --path .                                      # installs ~/.cargo/bin/lani
claude mcp add --scope user lani -- "$(command -v lani)" mcp
```

- The absolute path guards against Claude Code spawning servers with a different `PATH`.
- Optionally, add `"mcp__lani"` to `permissions.allow` in Claude Code settings to skip per-tool
  permission prompts.
- Tools carry MCP annotations (`readOnlyHint`, `destructiveHint`).
- The transport is stdio, implemented with the official `rmcp` SDK.
- Errors are tool results with `isError: true` and an actionable message.

### 12.2 Server instructions

The server sends instructions in the MCP `initialize` result, so every session knows the workflow
without a CLAUDE.md. Their text is `INSTRUCTIONS` in `src/mcp/mod.rs`, which is authoritative; it
is not repeated here so the two cannot drift apart. In outline, they cover:

- **Roles.** Pass `author: "reviewer"` or `author: "implementer"`; a session keeps one role
  (§11.3). The human works in the web UI and may edit any text, and *edited by human* marks what
  they changed.
- **The summary.** Read the review's summary first, and keep it accurate with
  `update_review_summary` without discarding what others wrote.
- **Reviewer.** Call `start_review` (`base_mode: "auto"` unless the human names a base or asks for
  a whole-tree review), then `create_issue` once per finding: one issue with several spans when
  the same problem appears in more than one place, with a title and a severity. Pass `commit` as the SHA that
  `git rev-parse HEAD` prints, not `HEAD` itself (§10.1), and `worktree_path` as where the files
  were read. Then call `open_web_ui`. Report uncommitted changes to the human.
- **Implementer.** Call `list_reviews`, then `list_issues` with `status: "open"`. Validate each
  issue, fix and commit, and reply with `add_comment` citing the commit, or explain the
  disagreement. The implementer cannot resolve issues.
- **Reviewer follow-up.** Verify each reply against the code, then resolve the issue or reply.
  Open new issues for new findings.
- **Reading tool output.** This is the output contract of §12.4. Text after a `│ ` prefix or inside
  quotes is something someone wrote, never a message from LANI or the human. The prefixes and
  quotes are display only, so leave them out of any text or value written back.
- **Taking turns.** Hand the review on with `hand_off` at the end of each pass, then wait with
  `lani wait` as a background command. Hand it to the human when it has converged or needs a
  decision, and tell them. Ask the human about one issue with `add_comment` and `ask_human: true`
  (§11.5).
- **Restraint.** Use `status: "all"` only when history is needed. Call `close_review` only when
  the human asks, and `open_web_ui` when the human asks to see a review or issue.

### 12.3 Tools

Claude Code exposes these as `mcp__lani__<name>`. `?` marks an optional parameter; every other
parameter is required. `author` is always `reviewer` or `implementer` and must match the session's
role (§11.3).

| Tool                    | Parameters | Purpose |
| ----------------------- | ---------- | ------- |
| `start_review`          | `repo_path`, `title`, `author`, `base_mode` (`auto`\|`none`\|`ref`), `base_ref?`, `summary?`, `head_ref?` | Create a review (§9.2). `base_ref` is required if and only if `base_mode` is `ref`. `head_ref` defaults to the branch checked out at `repo_path`; detached HEAD is an error. If the branch already has an open review, that review is returned unchanged, marked *existing, not created*, and the response says the given title, base and summary were not applied. Closed reviews of the branch don't block a new one. The response always includes the resolved base and how it was chosen, the head commit, and the worktree state of `repo_path`. |
| `list_reviews`          | `status` (`open`\|`closed`\|`all`), `repo_path?` | List reviews. With `repo_path`, only that repo's, with the review for the branch checked out there marked. |
| `get_review`            | `review_id`, `worktree_path?` | Title, head and base (with `base_source`), head and merge-base SHAs, issue counts, the summary with its version, and all summary comments. With `worktree_path`, that worktree's state. |
| `update_review_summary` | `review_id`, `author`, `summary`, `expected_version` | Replace the summary. Rejected if it changed after `expected_version`. |
| `list_issues`           | `review_id`, `status` (`open`\|`resolved`\|`all`), `detail` (`summary`\|`full`), `path?` | **R5.** `summary` gives one line per issue. `full` gives the anchor, current or original code, and the whole comment chain. `path` keeps the issues in that file: the file an issue was opened on, or the file its lines are in now after a rename. The header's "not shown" count of issues left out by `status` covers the same file, or the whole review without `path`. |
| `get_issue`             | `issue_id` | One issue in `full` detail. |
| `create_issue`          | `review_id`, `author`, `title`, `severity` (`high`\|`medium`\|`low`\|`nit`), `body`, `spans` (1–20 of `path`, `side` (`head`\|`base`), `start_line`, `end_line`, `start_line_text`), `commit`, `worktree_path` | §10.1. `title` is one line naming the problem, without the severity. List the main span first. `worktree_path` is a string or explicitly `null`. |
| `add_comment`           | `author`, `body`, `issue_id?`, `review_id?`, `ask_human?` | Exactly one of `issue_id` (reply on an issue) or `review_id` (comment on the summary). With `issue_id`, `ask_human: true` makes the comment a question for the human (§11.5). |
| `hand_off`              | `review_id`, `author`, `to` (`reviewer`\|`implementer`\|`human`), `note?` | §11.5. Only on the session's turn, or while it is nobody's. The reply says how to wait with `lani wait`, or to tell the human. |
| `edit_comment`          | `comment_id`, `author`, `body`, `expected_version` | Only comments written by the same role. |
| `set_issue_status`      | `issue_id`, `author`, `status` (`open`\|`resolved`), `comment?` | Reviewer only. The comment is published with the change and is required when reopening. |
| `close_review`          | `review_id`, `author` | Reviewer only, and only when you ask. |
| `open_web_ui`           | `review_id?`, `issue_id?`, `view?` (`overview`\|`changes`\|`files`), `path?`, `line?` | **R9.** Ensures the server is running (§13.1) and opens your browser at that page; with no arguments, the review list. `view`, `path` and `line` need `review_id`. `path` alone means the Files view, and `line` needs `path`. A line opens selected: in the Files view, or on the head side of the file's diff. The Changes view is refused for a whole-tree review. Returns the URL *without* the token. If no browser can be launched (e.g. over SSH), it says to run `lani open … --print-url`. A path goes into that command only if it is plain enough to be safe in a shell; otherwise the message gives the page's address to visit after signing in. |

### 12.4 Output format

Tool results are compact Markdown-like text rather than JSON, because that uses fewer tokens and
reads well. IDs and versions are always explicit. Code is shown at the issue's current location,
or as the stored snippet when the issue is outdated.

**Output contract.** Claude sees only this text, and it mixes LANI's own structure with text that
other parties control: the human, the other Claude role, and the repository (titles, file paths,
branch and repository names, code). That text must never pass for LANI's structure, for example a
forged `[c99 v1] human · …` comment header, or a title that closes its quotes and adds
`· resolved by human`. The rules:

1. **Multi-line text** (comments, summaries, and the current text in edit-conflict errors) appears
   only in blocks whose every line starts with `│ `. Bodies are split at every character a reader
   might treat as a line break (`\n`, `\r`, VT, FF, NEL, U+2028, U+2029), not just `\n`.
2. **Single values inside LANI's lines** are double-quoted with backslash escapes (Rust's `{:?}`
   format), so a value can neither end early nor contain a line break. This covers titles, file
   paths, branch and repository names, worktree paths, issue previews, and the text of git errors.
   Values from the user's own environment (`LANI_HOME`, `PATH`, `BROWSER`, the git version) are
   not quoted.
3. **Code excerpts** sit behind a line-number gutter, inside a fence longer than any run of
   backticks in the code.
4. **Final pass.** Every tool result and error gets one more pass that turns any line break other
   than `\n` into a visible `␤`, as a backstop.
5. **Display only.** Prefixes and quotes are not part of the stored text or of any name, and the
   server instructions say so. Copying them back in is flagged, never silently corrected:
   - **Written text** (`summary`, `body`, `comment` in `start_review`, `update_review_summary`,
     `create_issue`, `add_comment`, `edit_comment` and `set_issue_status`): each parameter's
     description says to leave the `│ ` prefixes out. A write in which every non-empty line
     starts with `│` is stored exactly as sent, and its result carries a note that the prefixes
     were probably copied. It is not rejected, because box-drawing text can legitimately look like
     that.
   - **Single values** (`title`, `base_ref`, `head_ref`, `repo_path`, `worktree_path`, `path`):
     each description says to pass the plain value. When a call fails, or a `list_issues` path
     filter matches nothing, while one of them is wrapped in double quotes, the result names the
     parameter. A quote-wrapped title is stored as sent, with the same note. `start_line_text`
     says to leave out the code-excerpt gutter.
6. **Enforced by one test.** `planted_text_never_appears_as_lani_structure` plants hostile values
   in every field people or the repository control. It calls every tool through the real binary,
   as both roles, including error results, and fails if planted text appears anywhere except in a
   `│ ` block, a code line, or an escaped quoted value. On the code before this contract it reports
   21 violations.

Titles also reject line breaks and control characters.

`start_review(..., base_mode="auto")` on a stacked branch with uncommitted edits:

```
Created review 5 "Retry policy for the API client" · started by reviewer
head "feature/retry" @ 5d6e7f8 · base "feature/rate-limit" (auto: nearest parent branch, 2 commits)
other candidates: "main" (6 commits)
Worktree "/Users/you/src/app": HEAD 5d6e7f8 (= review head) · uncommitted changes: "src/retry.rs"
Only committed code is reviewed; issues cannot anchor to lines affected by uncommitted changes.
```

`get_review(review_id=3)`:

```
Review 3 "Add token-bucket rate limiter" · open · started by implementer · repo "app"
head "feature/rate-limit" @ 3f9c2a1 · base "main" (auto-detected) · merge-base 9e8d7c6 · 4 commits
issues: 3 open, 5 resolved

## Summary (v3 · last edited by human at 2026-10-07T13:40Z)
│ Adds a token-bucket rate limiter in front of the public API so that one client cannot starve the
│ others. Limits are configured per API key; existing keys keep today's behaviour.

## Summary comments
[c190 v1] reviewer · 2026-10-07T13:55Z · at 3f9c2a1
│ The approach looks right. Main concerns are overflow and burst test coverage (issues 57 and 58).
```

`list_issues(review_id=3, status="open", detail="summary")`:

```
Review 3 "Add token-bucket rate limiter" · "feature/rate-limit" → "main"
head 3f9c2a1 · merge-base 9e8d7c6 · showing 3 open · not shown: 5 resolved

Issue 57 · open · high · "Refill overflows u64 after a long idle period" · "src/limiter.rs":42-44 (outdated) · 2 comments, last: implementer
Issue 58 · open · medium · "No test covers a burst larger than capacity" · "src/limiter.rs":88-90 · 1 comment, last: reviewer
Issue 61 · open · nit · "Document the refill algorithm" · "README.md":12 · 1 comment, last: human
```

For a whole-tree review the header reads, for example, `Review 4 "Audit of main" · "main" (whole
tree) · head 77ab12c`.

`get_issue(issue_id=57)`:

````
Issue 57 · open · high · review 3 · opened by reviewer
Title: "Refill overflows u64 after a long idle period"
Location: "src/limiter.rs":42-44 (head side) @ a1b2c3d — OUTDATED: the lines changed between a1b2c3d and 3f9c2a1
Original code @ a1b2c3d:
```rust
  39 |     pub fn try_acquire(&mut self) -> bool {
  40 |         let now = Instant::now();
  41 |         let elapsed_ms = (now - self.last).as_millis() as u64;
> 42 |         let refill = elapsed_ms * self.rate;
> 43 |         self.tokens = (self.tokens + refill).min(self.capacity);
> 44 |         self.last = now;
  45 |         if self.tokens == 0 {
  46 |             return false;
  47 |         }
```

[c201 v2] reviewer · 2026-10-07T14:02Z · at a1b2c3d · edited by human
│ `elapsed_ms * self.rate` overflows u64 after a long idle period; use `saturating_mul`.

[c215 v1] implementer · 2026-10-07T14:31Z · at 3f9c2a1
│ Fixed in 3f9c2a1: switched to `saturating_mul` and added `refill_after_long_idle` test.
````

An issue with several spans lists them, and labels each span's code, `Span 2, Code @ 3f9c2a1:`:

```
Issue 63 · open · medium · review 3 · opened by reviewer
Title: "Bucket capacity is read without the lock"
Locations:
  1. "src/limiter.rs":31 (head side) @ 3f9c2a1
  2. "src/limiter.rs":72-73 (head side) @ 3f9c2a1
  3. "src/admin.rs":18 (head side) @ 3f9c2a1
```

In summary lists, such an issue shows its first span and how many more it has:
`"src/limiter.rs":31 + 2 more spans`.

A rejected anchor (§10.1, step 3), returned with `isError: true`:

```
create_issue rejected: "src/limiter.rs" on disk in "/Users/you/src/app" differs from commit 3f9c2a1.
The differences are above lines 42-44, so LANI cannot tell which version your line numbers came from.
  - If you read the file on disk: the same lines are 39-41 in 3f9c2a1. Resubmit with those numbers.
  - If you read the committed file (git show): resubmit with worktree_path: null.
Uncommitted changes are not part of the review; they must be committed to be reviewed.
```

---

## 13. Web UI

### 13.1 Server lifecycle, discovery and idle shutdown (R9, R16)

**Finding or starting the server.** `lani open [target]` and the `open_web_ui` tool share one
routine:

1. Read `run/server.json`. If it exists, call `GET http://127.0.0.1:<port>/api/health` with
   `Authorization: Bearer <token>` and a 500 ms timeout. The response must carry the same
   `instance_id`, which guards against another process reusing the port. On success, go to step 4.
2. Otherwise, start the server: re-exec `current_exe()` as `lani serve --detach` in a new process
   group, with stdin from `/dev/null` and stdout and stderr going to `logs/server.log`. The server
   ignores SIGHUP, so it survives both the Claude session and the terminal that started it.
   On Linux, `current_exe()` reads `/proc/self/exe`, which shows `<path> (deleted)` once the
   binary has been replaced on disk, as `cargo install` does. LANI then runs `<path>`, the
   replacement, and the version-mismatch warning below covers the difference. If neither path
   exists, the error says the binary was removed or replaced and the process must be restarted.
3. Wait up to 5 s for `server.json` to show a new `instance_id` that passes the health check.
4. Open the browser (§13.2).

**Server startup.**

- Take an exclusive `File::try_lock` on `run/server.lock`. If another server holds it, exit 0. This
  makes concurrent `open` calls race-free, and the OS releases the lock if the server crashes.
- Bind `127.0.0.1:0` (an ephemeral port), or `--port N`.
- Generate a 256-bit token and a 128-bit `instance_id` from the OS RNG.
- Write `server.json` atomically (temp file + rename, mode 0600), then serve.

**Shutdown.** The server shuts down on `POST /api/shutdown`, SIGTERM, SIGINT or idle timeout. In
every case it removes `server.json` if the file is still its own. `lani stop` calls `/api/shutdown`
with the bearer token, so no signal-sending code is needed.

**Version mismatch.** If the running server's version differs from the caller's, `open` prints a
warning suggesting `lani stop && lani open`. The MCP tool includes the same warning in its result.

**Idle shutdown.**

- **Default and values.** The server stops after **3 hours** without activity. The timeout can be a
  duration (`30m`, `3h`, `2d`; minimum 5 minutes) or `never`.
- **What counts as activity.** Any authenticated HTTP request. An open live-update (SSE)
  connection does *not* count, so a forgotten browser tab cannot keep the server alive forever.
- **Setting it.** There are two places:
  - The **web UI** settings page (`/settings`) saves the value in the `setting` table. It applies
    to the running server immediately and to every future start.
  - **`lani serve --idle-timeout <value>`** overrides the saved value for that server run only.
  - For the running server, the most recent change wins. The settings page shows the effective
    value and where it came from (saved setting or command-line flag).
- **Before stopping.** The server sends a final `stopping` message on open SSE connections. Pages
  then show: *"LANI stopped after 3 h idle. Run `lani open`, or ask Claude to open it again."*
- **Status.** `lani status` shows the effective timeout and the time remaining.

### 13.2 Authentication and browser security (R10)

**Token.** Each server start generates a fresh 256-bit random token, hex-encoded. It is stored only
in `server.json` (mode 0600 in a 0700 directory) and in server memory, and it is compared in
constant time.

**Launching the browser without exposing the token.** On Linux, any local user can read process
arguments, so passing the URL to `xdg-open` would leak the token. LANI uses Jupyter's technique
instead:

1. Write `run/open-<random>.html` (mode 0600) containing a meta-refresh to
   `http://127.0.0.1:<port>/login?token=<token>&next=<path>`.
2. Open *that file* with `$BROWSER` if set, otherwise `open` (macOS) or `xdg-open` (Linux).
3. The server deletes `open-*.html` files older than 60 s, and all of them when it stops.

**Session.** `/login` validates the token, sets the cookie, and answers with a small sign-in page
that continues to `next` by itself (a meta refresh, plus a "Continue" link as a fallback). The
cookie is `lani_<port>=<token>` with `HttpOnly`, `SameSite=Strict` and `Path=/`. `next` must be a
local path that starts with `/` and not `//`, using only letters, digits and `-._~/?=&#%+`;
anything else becomes `/`. The port is in the cookie name because browsers do not isolate cookies
by port.

- **Why Strict, not Lax.** Browsers send 127.0.0.1 cookies to every port on that host. A Lax
  cookie travels with any cross-site top-level navigation, so a website that sends the browser to
  `http://127.0.0.1:<another port>/` delivers the token to whatever listens there on the very
  first request. Strict withholds the cookie from that first, cross-site request, but it does
  **not** keep the cookie away from other loopback ports. SameSite compares sites, and the port is
  not part of a site. Once a page from another loopback port is showing, its own requests are
  same-site and carry the cookie: a reload, a second navigation, an image, even the browser's
  automatic favicon fetch. Strict only stops a listener that logs the first request; a hostile
  server defeats it with one more request. The remaining exposure is accepted (§15). M2 briefly
  used Lax, and the first LANI review caught that.
- **Why a sign-in page instead of a redirect.** `lani open` reaches `/login` from a local redirect
  file, which browsers treat as a cross-site origin. They withhold Strict cookies for that whole
  navigation, including any HTTP redirect, so a 303 to `next` would arrive signed out. The
  sign-in page's own navigation starts from a 127.0.0.1 page, so it is same-site and carries the
  cookie.

**Request checks.** Every route except `/login` requires all of the following:

- A valid cookie or `Authorization: Bearer <token>`.
- A `Host` of `127.0.0.1:<port>` or `localhost:<port>`, which blocks DNS rebinding. Any other
  `Host` gets 421 Misdirected Request.
- For state-changing methods, an `Origin` matching the server, or `Sec-Fetch-Site: same-origin`.
  This is CSRF defense in depth. `Origin: null` counts as no `Origin`, so `Sec-Fetch-Site`
  decides.

**Response headers.**

- `Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'
  data:; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'`.
  This forbids inline scripts and styles. It also blocks remote images in comment Markdown, which
  keeps the "No Internet" promise in the browser too.
- `X-Content-Type-Options: nosniff`, `Referrer-Policy: same-origin`, and `Cache-Control: no-store`
  on HTML.
- `Referrer-Policy` is `same-origin`, not `no-referrer`. Under `no-referrer`, browsers send
  `Origin: null` on form posts, and LANI's own forms were refused (found in the first LANI
  review). `same-origin` still sends no referrer to other sites.

**Logging.** Query strings are never logged, so the `/login` token never appears in logs.

### 13.3 Pages and URLs

| URL                                         | Page |
| ------------------------------------------- | ---- |
| `/`                                         | **Every open review across all repos (R17)**, grouped by repo, with issue counts. A toggle includes closed reviews. |
| `/reviews/{id}`                             | **Overview.** Title (editable); head, base picker (§9.5) with an *auto-detected* label when applicable, merge-base and commit count; **summary** (rendered Markdown, editable, shows who last edited it); **summary comments** (linear thread with a comment box); issue list with filters (open/resolved, author, file); close/reopen review; activity timeline. Every review page also shows whose turn it is with the hand-off note, and the questions waiting for you as links (§11.5); the overview lists those issues first and has a *Hand on* control. |
| `/reviews/{id}/changes`                     | **Diff** from merge-base to head (diff reviews only). Changed-file sidebar with +/− stats and issue counts, then a unified diff per file with inline issue threads. |
| `/reviews/{id}/files/{dir…}/`               | **Tree** at head. Directory listing that marks changed files (diff reviews) and files with issues. |
| `/reviews/{id}/files/{path}#L42-L48`        | **File** at head with highlighting, inline threads and line-range selection. |
| `/reviews/{id}/issues/{issue_id}`           | **Issue.** Code context (current or outdated snippet), full thread, and links to the file and diff views. This is the deep-link target for `open_web_ui(issue_id)`. |
| `/reviews/{id}/events`                      | Server-Sent Events stream (§13.5). |
| `/issues/{issue_id}`                        | Redirects to the issue's page under its review, so `lani open --issue` and other links need only the issue ID. |
| `/settings`                                 | Idle timeout (§13.1); round limit (§11.5); server version, pid, port and uptime. |
| `/login`, `/api/health`, `/api/shutdown`    | §13.1, §13.2. |

The pages are backed by state-changing endpoints. They are plain HTML form posts that redirect
back to the page (303 See Other); M4 may add htmx fragments for live updates:

- Create an issue (`POST /reviews/{id}/issues`, from a line selection in a code view).
- Add a comment, on an issue or on the summary.
- Edit a comment or the summary (with `version`).
- Change the title or base.
- Hand the review to a Claude role, or to nobody (`POST /reviews/{id}/turn`).
- Resolve or reopen an issue.
- Close or reopen the review.
- Update settings: the idle timeout, and the round limit (`POST /settings/round-limit`).
- Render a Markdown preview (`POST /preview`, which returns an HTML fragment for an editor's
  *Preview* tab).

### 13.4 Interactions

- **Selecting lines.** Click a line number to select it, and shift-click another on the same side
  to extend the range. A button under the range opens the new-issue form.
  - **Diff sides.** In the Changes view, removed lines select side `base` by their old numbers.
    Context and added lines select side `head` by their new numbers.
  - **Links.** The URL hash follows the selection: `#L42-L48` in the Files view, and
    `#file-<path in hex>-H42-H48` (head side) or `-B…` (base side) in the Changes view. Opening
    such a link restores the selection.
  - **JavaScript.** Selection needs it; without it, line numbers are plain links to the line.
- **New issue form.** A Markdown body with *Write/Preview* tabs and a *Create issue* button. The
  form carries the head commit the page was rendered at, which becomes the anchor commit (§10).
  The server runs steps 1 and 2 of §10.1, since the lines come from git.
  - **Base side.** A selection also carries the merge-base the page showed. If the base has moved
    since, the merge-base and its line numbers may differ, so the issue is rejected and you
    reload.
  - **After creating.** The browser returns to the view, at the new thread.
- **Authorship display.** Described in §11.1. Every issue, comment and the summary shows the
  author's badge, plus *edited by you* or *edited* markers.
- **Threads.** Comments are linear, oldest first, with a comment box at the bottom. Any comment can
  be edited in place. Issues have *Resolve* / *Reopen* buttons. Threads shown in a code view
  send the view's address with each form, so a reply, edit or status change returns there. Only
  a local LANI path is accepted as that address; anything else is ignored.
- **Edit conflicts.** As described in §11.4, nothing is silently lost.
- **Issue placement.** In the file and diff views, a thread renders below the (relocated) end line
  of a span. An issue appears only once per page, however many spans it has.
  - **Files view:** the thread goes under the issue's first current span in the file, in line
    order.
  - **Changes view:** the thread goes under the first span on the page, in page order.
  - **Markers:** each later span gets a small marker, `◆ #12`, at the right of its last line. It
    links to the thread, and its tooltip gives the issue's number and title.
  - **No placeable span:** an issue with no span on a shown line is listed in a box at the top of
    its file (in the Changes view, its first file on the page), with every span's snippet. That
    covers outdated issues and, in the diff view, spans outside the hunks.
  - **Issue page:** lists every span with its location, its code, and links to both views: to
    the Files view for a head-side span, and to the section of the Changes view that shows the
    span's file, when the review's diff changes it.
  - **Files tree:** counts an issue once for a file or directory, however many of its spans are
    inside.
  - **Creating:** a new issue from the web UI has one span, the selected range.

### 13.5 Live updates

- **Polling.** The web server polls `SELECT max(id) FROM event` every 500 ms, but only while at
  least one SSE client is connected.
- **Push.** When the value grows, the server reads the new events and pushes a `changed` message,
  listing issue IDs and whether the summary changed, to the affected reviews' streams.
- **Refresh.** The page refreshes those parts.
- **Open editors.** A thread or summary with an open editor is never replaced. It shows an
  *"updated elsewhere — reload"* notice instead.
- **Idle timer.** SSE connections do not count as activity (§13.1).

### 13.6 Syntax highlighting and diffs (R8)

- **Engine.** syntect with the `two-face` syntax and theme set (what `bat` uses): roughly 200
  languages, including TypeScript, TOML and Dockerfile.
  - It uses the Oniguruma regex backend, syntect's default and the one with the broadest syntax
    compatibility.
  - Output uses CSS classes rather than inline styles, which keeps it CSP-compatible and allows
    light and dark themes through `prefers-color-scheme`.
  - The theme CSS is generated once and served as a static file (`/static/highlight.css`). It
    uses InspiredGitHub for light mode and OneHalfDark for dark mode. Each theme's rules sit
    inside its own media query, because one theme styles scopes the other leaves alone, and a
    light colour must not leak into dark mode.
- **Language detection.** By file name or extension, then by the first line (shebang or modeline),
  falling back to plain text.
- **Per-line output.** The whole file is highlighted with syntect's line-by-line parser, so
  multi-line constructs are correct. Each line is emitted as a balanced HTML fragment, with scopes
  re-opened at the start of each line. The file view, diff rows and snippets all index into this
  per-line array.
- **Caching.** An in-memory LRU, capped at 32 MiB of HTML, keyed by blob SHA and file name. The
  file name is part of the key because it selects the language, so the same blob under two names
  can highlight differently. Blobs are content-addressed, so cached entries are never stale. The syntax set loads lazily on first use, at an estimated
  20–60 ms once per server lifetime.
- **Limits.**
  - Files over 1 MiB or 20,000 lines render as plain text.
  - Files over 10 MiB are not shown at all, only described.
  - Files with a NUL byte in the first 8 KiB are shown as "binary file".
  - Highlighted HTML is large, about 0.8 KB per line, because each line re-opens its full scope
    stack. The M4 benchmarks decide whether that needs trimming.
  - Diffs of files with more than 1,500 changed lines are collapsed behind a "Load diff" link,
    which reloads the Changes view with `?expand=<path>`.
- **Diffs.** `git diff` output (§8) is parsed into files → hunks → rows. Each row's HTML comes from
  the highlighted old or new blob, by line number. Diffs are unified only in v1. A page runs one
  `git diff` per changed file, plus one `cat-file --batch-check` for every blob's size and one
  `cat-file --batch` for the blobs it shows, rather than several processes per file.

### 13.7 Markdown

Comments and summaries are rendered with **comrak**:

- GitHub-flavored extensions are on: tables, task lists, strikethrough and autolinks.
- Raw HTML is disabled, which is comrak's default. That setting also drops dangerous link schemes
  such as `javascript:`.
- Fenced code blocks use the same syntect highlighter, chosen by the fence's language token
  (` ```rust `); unknown languages are shown as plain text.
- Links open in the same tab, and `Referrer-Policy: same-origin` keeps LANI URLs from leaking to
  linked sites, so no `rel` attributes are needed.
- Markdown is rendered on every request. A cache keyed by (object, version) is deferred until
  the M4 benchmarks show rendering matters (§21).

### 13.8 Frontend technology

- **askama** renders the pages on the server. Its templates are compiled into the binary and
  type-checked, with HTML autoescaping on by default. `|safe` is used only for HTML produced by
  comrak or syntect.
- **htmx 2** and its SSE extension are planned for M4's partial updates. They would be vendored
  and pinned, running under the CSP with `allowEval: false`, `allowScriptTags: false` and
  `includeIndicatorStyles: false`. M3 needs no partial updates, so it does not ship htmx.
- One small vanilla JS file, `static/lani.js`, handles line selection and the editor's
  *Write/Preview* tabs (M3), and in M4 SSE-driven refresh and editor protection. It has no inline
  code and sets no inline styles, so it runs under the CSP. The code views load it; other pages
  don't need it.
- One hand-written CSS file covers light and dark themes.
- **No Node toolchain.** `cargo build` is the whole build.
- **Forms work without JavaScript.** Every action is a plain form post that redirects back, and
  inline editors are `<details>` elements. Only line selection needs `lani.js`.

---

## 14. CLI

```
lani mcp                                    MCP server on stdio (spawned by Claude Code)
lani open [--review ID [--view overview|changes|files] [--file PATH[:LINE]] | --issue ID]
          [--print-url] [--no-browser]      open the web UI, starting the server if needed
lani serve [--port N] [--idle-timeout DURATION|never] [--detach]
                                            run the web server (foreground unless --detach)
lani status                                 server pid, port, version, uptime, idle timeout and time left
lani stop                                   stop the running server
lani wait --review ID --for reviewer|implementer [--timeout DURATION]
                                            wait until it is that role's turn (§11.5); exits 0 on
                                            its turn, 3 if the review is closed, 2 on timeout
lani version
```

- `--print-url` writes the full tokenized URL to stdout. This is useful over SSH with port
  forwarding.
- `--file` opens the Files view unless `--view changes` is given. A trailing `:<digits>` is a line
  number, which opens selected; any other colon is part of the path.
- There is no config file. The environment variables are `LANI_HOME`, `BROWSER`, and `LANI_LOG`
  (a `tracing` filter for stderr logging, e.g. `debug`; default `warn`). The only persisted
  setting is the idle timeout (§13.1).
- An existing `$LANI_HOME` that other users can access is refused with a `chmod 700` hint rather
  than silently changed.

---

## 15. Security model

| Threat                                                   | Mitigation |
| -------------------------------------------------------- | ---------- |
| Another local user connects to the web port              | Token required on every request; 256-bit random value with constant-time comparison. |
| Token read from another process's arguments (`ps`)       | The token never appears on a command line; the browser opens a 0600 redirect file instead. |
| Token read from disk                                     | `server.json` is mode 0600 inside the 0700 `$LANI_HOME`; the DB is also 0600. |
| DNS rebinding from a malicious website                   | `Host` header allowlist; the server binds 127.0.0.1 only. |
| CSRF from a malicious website                            | `SameSite=Strict` cookie, plus an `Origin` or `Sec-Fetch-Site` check on every POST; GETs have no side effects. |
| XSS via comment text written by Claude or pasted content | Autoescaping templates, raw HTML disabled in Markdown, dangerous URL schemes dropped, strict CSP with no inline scripts. |
| Git option injection via refs or paths                   | `--end-of-options` or `--` before all user-supplied values; refs verified with `rev-parse --verify`. |
| Path traversal in the file browser                       | Files are read only through git objects (`<commit>:<path>`), never from the filesystem. |
| Claude acting as you                                     | MCP accepts only `reviewer` and `implementer` authors. |
| Text from one party passing for another in Claude's view | The output contract (§12.4): `│ ` blocks for multi-line text, escaped quoted values, a final line-break pass, and one end-to-end test across every tool. |
| An implementer session resolving issues                  | Permission check in the service layer, DB CHECK on `resolved_by`, per-session role lock (§11.3). |

Accepted residual risks:

- **The token can reach another server on loopback.** The `lani_<port>` cookie holds the token,
  and browsers send 127.0.0.1 cookies to every port. `SameSite=Strict` withholds it only from
  cross-site requests (§13.2). Whenever the browser shows a page served from another loopback
  port, that page's own requests carry the cookie, and whoever runs that server gets the token.
  - **Triggers:** any website can arrange this in two hops: it navigates the browser to the
    hostile port, and the hostile page makes one more request to itself. Opening such a page
    directly, or following a link to it from a LANI page, also works.
  - **Who can exploit it:** a process that can listen on a loopback port but cannot read
    `server.json`. That means other users on a shared machine, or sandboxed or containerised
    local processes.
  - **Who cannot gain from it:** on a single-user machine without such sandboxes, anything that
    can listen on loopback can already read `server.json`, so this adds no exposure there.
  - **Limits:** the cookie stops working when the server stops. Jupyter has the same exposure.
    Hardening is deferred (§21).
- **Token in browser history.** The `/login?token=…` URL may be recorded in your browser history,
  which is private to your OS user.

---

## 16. Performance budget

These are targets. Figures marked *est.* are estimates to be verified with benchmarks during M1
and M3.

| Metric                                         | Target       | Notes |
| ---------------------------------------------- | ------------ | ----- |
| `lani mcp` start → ready                       | < 20 ms      | est. ~5 ms: opening the DB and checking `user_version`. |
| MCP tool call (excluding git)                  | < 10 ms p95  | Single-digit SQLite queries. |
| MCP tool call with anchor validation           | < 50 ms p95  | 2–4 git subprocesses at est. 3–10 ms each. |
| `start_review` with parent detection           | < 100 ms     | One `for-each-ref` call computes the distance to every local branch. |
| `lani open` with a server already running      | < 100 ms     | Health check, redirect file, and spawning the opener. |
| First render of a 2,000-line file              | < 250 ms     | Measured in M3: 47 ms in a release build, including loading the syntax set; dominated by syntect, then cached. |
| Cached page render                             | < 30 ms      | Measured in M3: 13 ms for that file (a 1.6 MB page). |
| RSS: `lani mcp`                                | < 15 MB      | |
| RSS: `lani serve`                              | < 100 MB     | Includes the 32 MiB highlight cache cap and the syntax set. Measured in M3: 17 MB after rendering that file. |
| Idle CPU                                       | ~0           | MCP blocks on stdin. The server polls only while SSE clients are connected, and exits after the idle timeout. |
| Release binary size                            | < 25 MB      | Measured in M3: 9.2 MB with LTO and strip (6.9 MB before the syntax set). |

---

## 17. Code layout and dependencies

There is one Cargo package with a library target (used by the integration tests) and a `lani`
binary.

```
Cargo.toml
src/
  main.rs            CLI (clap) → dispatch
  lib.rs
  paths.rs           $LANI_HOME resolution, directory and file permissions
  store/             rusqlite: connection setup, migrations/*.sql, typed queries
  git.rs             git subprocess wrapper; unified-diff, name-status and for-each-ref parsers
  base.rs            default-branch and parent-branch detection (§9)
  anchor.rs          anchor validation, snippet capture, relocation (§10)
  service/           domain operations, permissions, role rules (§11), shared by MCP and web
    mod.rs           reviews, issues, comments, anchors and placement
    code.rs          what the code views show: directories, files and diffs at the head
  mcp/               rmcp tool router, server instructions, role lock, text rendering (§12.4)
  web/
    mod.rs           axum router, middleware (auth, Host, Origin, CSP, activity tracking)
    auth.rs          token, cookie, Host and Origin checks; sign-in
    pages.rs         full-page handlers
    code.rs          the Files and Changes views
    actions.rs       form posts
    views.rs         view models and the error page
    sse.rs           event polling and fan-out (M4)
    templates/       askama templates (compiled in)
    static/          lani.js, lani.css (embedded); htmx.min.js and htmx-ext-sse.js in M4
  highlight.rs       syntect + two-face, per-line output, LRU cache
  markdown.rs        comrak configuration
  daemon.rs          server.json, lock, detach/spawn, health check, browser launch, idle timer
tests/               integration tests (temp git repos, MCP client, HTTP); browser.rs is opt-in
clippy.toml          words doc comments may use without backticks, such as WebDriver
```

| Concern            | Crate(s) |
| ------------------ | -------- |
| CLI                | `clap` (derive) |
| SQLite             | `rusqlite` with `bundled`; STRICT tables need SQLite ≥ 3.37 |
| MCP                | `rmcp` (official SDK; stdio transport, tool macros) + `schemars` |
| Async runtime      | `tokio`: current-thread runtime for `mcp`, multi-thread for `serve` |
| HTTP server        | `axum`, with LANI's own middleware for auth and security headers |
| HTTP client        | A small blocking HTTP/1.1 client over `std::net::TcpStream` (`daemon.rs`), used only for LANI's own fixed-length health and shutdown responses |
| Templates          | `askama` |
| Highlighting       | `syntect` + `two-face` |
| Markdown           | `comrak` |
| Errors             | `thiserror` in the library, `anyhow` at the CLI edge |
| Logging            | `tracing` + `tracing-subscriber`; MCP logs to stderr only, because stdout carries the protocol |
| Time and durations | `jiff` |
| Randomness         | `getrandom` |
| Constant-time eq   | `subtle` |
| LRU cache          | `lru` |
| Serialization      | `serde`, `serde_json` |

Linting and safety rules:

- **Formatting and lints.** `cargo fmt`, plus `cargo clippy --all-targets -- -D warnings` with
  `clippy::pedantic` enabled in the `[lints]` table. Every `#[allow]` carries a justification.
  Two exceptions so far:
  - `missing_errors_doc` is allowed crate-wide. Every fallible function returns the crate's single
    `Error` type, so per-function `# Errors` sections would add nothing.
  - `unused_async_trait_impl` is allowed on the `ServerHandler` impl, because rmcp's
    `#[tool_handler]` macro generates those functions.
- **No unsafe code.** `unsafe_code = "forbid"` holds without exceptions:
  - Detaching uses `CommandExt::process_group(0)` and ignores SIGHUP.
  - Locking uses `std::fs::File::try_lock` (Rust ≥ 1.89).
  - Stopping the server goes over HTTP.

Dependency versions are pinned in `Cargo.lock` when implementation starts.

---

## 18. Build and distribution

- **Release profile:** `lto = "thin"`, `codegen-units = 1`, `strip = true`. Panics unwind, so a bug
  in one handler cannot take down the server.
- **Local install:** `cargo install --path .`
- **Release targets:** `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-musl`
  and `aarch64-unknown-linux-musl`.
  - musl gives fully static Linux binaries.
  - They are built on per-OS CI runners or with `cargo-zigbuild`, because the bundled C code
    (SQLite, Oniguruma) needs a C cross-toolchain. CI and prebuilt releases are deferred (§21).
- **macOS Gatekeeper:** binaries downloaded through a browser are quarantined; locally built
  binaries are not. Signing is deferred.

---

## 19. Testing

- **Unit tests:**
  - Unified-diff, `-z` name-status and `for-each-ref` parsers, including renames, C-quoted paths,
    "No newline at end of file" and binary markers.
  - Relocation, as table-driven hunk walks.
  - `next`-path validation, and Host and Origin checks.
  - Duration parsing for the idle timeout (minimum value, `never`).
  - Markdown sanitization, using a corpus of hostile bodies: raw HTML, `javascript:` links, remote
    images, and template-syntax lookalikes.
- **Base detection:** real temp repos.
  - Branch from the default branch.
  - Stacked branches.
  - Parent already merged into the default branch.
  - A tie that includes the default branch, and a tie that doesn't.
  - Descendant branches excluded.
  - No candidates.
  - Head on the default branch (whole-tree review).
  - Each step of default-branch detection.
- **Anchor validation (§10.1):** real temp repos.
  - A commit not on the branch, and a branch rebased after the read.
  - A clean file.
  - Dirty hunks after, inside and before the region, with staged and unstaged edits.
  - A moved `HEAD`.
  - A file deleted on disk.
  - `worktree_path: null`.
  - A `start_line_text` mismatch.
  - A side-`base` issue on a whole-tree review.
- **Read-only guarantee:** every git invocation leaves `.git/index` byte-identical.
- **Service integration:** drive full implementer → reviewer → human cycles, including rebases,
  force-pushes and base changes. Coverage includes:
  - The full permission matrix (§11.2).
  - The role lock.
  - Version conflicts on comments and the summary.
  - The composite foreign key rejecting a comment whose issue belongs to another review.
- **MCP:** an in-process `rmcp` client over a duplex transport asserts the tool schemas and the
  exact text output.
- **Output contract (§12.4):** one end-to-end test plants hostile values in every field people or
  the repository control, calls every tool as both roles, and checks that no planted text appears
  as LANI's structure.
- **Web:** handlers exercised with `tower::ServiceExt::oneshot`.
  - Auth on every route, and the CSP and other security headers.
  - 409 on edit conflicts.
  - The base picker.
  - Settings precedence (saved setting vs flag).
- **Browser:** `tests/browser.rs` drives Safari through W3C WebDriver (`safaridriver`, which
  ships with macOS) against the real `lani` binary.
  - It covers selecting lines in the Files and Changes views, the new-issue form and its preview,
    head- and base-side issues (checked in the database too), and replying and resolving in
    place.
  - It also records any Content-Security-Policy violation or script error after each page loads.
    WebDriver cannot see console messages from loading itself.
  - It needs a browser, so it is opt-in: `cargo test --test browser -- --ignored`. Enable Safari's
    automation once: Settings → Advanced → "Show features for web developers", Develop → "Allow
    Remote Automation", then `safaridriver --enable`.
  - The WebDriver client is plain HTTP and JSON over `std`, so it adds no dependency and no Node
    toolchain. It was checked by breaking `lani.js` on purpose (shift-click ignored, an inline
    script injected); both breaks fail the test.
- **Turns (§11.5):** hand-off permissions, the round limit and the human resetting it, questions
  cleared by the human's reply or status change, the migration from version 2 keeping data and
  event ids, two MCP sessions taking turns, and `lani wait`'s three exits in a real process.
- **Idle timer:** tested with an injected clock.
  - Requests reset it; SSE connections don't.
  - `never` disables it.
  - The `stopping` message is sent.
- **Process:** the built binary.
  - `open --no-browser --print-url` starts a server, and a second call reuses it.
  - `stop` shuts it down.
  - Concurrent `open` calls produce exactly one server.
  - A crashed server (lock released, stale `server.json`) is replaced.
- **Concurrency:** two processes writing simultaneously, with no `SQLITE_BUSY` surfacing.
- **Test harness:** every process spawn in a test binary goes through one mutex. On macOS a pipe
  created by one thread can leak into a child forked concurrently by another thread; a
  long-lived LANI server inheriting such a pipe makes the first thread wait for EOF forever.
- **Coverage:** `cargo llvm-cov`.

---

## 20. Milestones

1. **M1 — Core + MCP.**
   - Scope: schema and migrations; git wrapper; base detection; anchors; service layer with
     permissions and the role lock; every MCP tool except `open_web_ui`.
   - Exit criteria: the full reviewer/implementer loop works from Claude Code in a real repo, for
     both diff and whole-tree reviews.
2. **M2 — Web foundation.**
   - Scope:
     - Daemon lifecycle with idle shutdown, plus the settings page.
     - Auth; `lani open`, `status`, `stop` and `open_web_ui`.
     - Pages: review list across all repos; overview with summary, summary comments, base picker
       and issue list; issue page.
     - Actions: commenting, editing, resolving. Markdown rendering.
   - Exit criteria: you can read and edit everything Claude wrote, change bases, and resolve
     issues.
3. **M3 — Code views.**
   - Scope: tree and file browser, highlighting, diff view, inline threads, line selection that
     opens a new issue.
   - Exit criteria: you can file your own issues from the file and diff views.
4. **M4 — Live updates and polish.**
   - Scope: SSE, editor protection, benchmarks against §16, README.

---

## 21. Deferred (not in v1)

- Exporting a review to a GitHub PR as review comments.
- Starting a review from the web UI.
- Deleting issues and comments. In v1, resolve the issue or edit the text instead.
- Edit history for comments and summaries.
- A per-repo override for the default branch (§9.3). For now, change the base in the web UI.
- A per-branch fallback for parent detection on git < 2.41.
- Interdiff: "changes since the last round".
- Side-by-side diff view, and expanding context lines in the diff. v1 links to the file view
  instead.
- Issue category labels ("correctness", "docs"…), and filtering issues by severity or category.
- Filtering the overview's issue list by author and by file (§13.3). The overview filters by
  status only; to be designed later.
- Suggested-change blocks that can be applied as patches.
- Fuzzy file finder and code search.
- A persistent `git cat-file --batch` process per repo, if subprocess cost shows up in benchmarks.
- Caching rendered Markdown per (object, version), if rendering cost shows up in benchmarks.
- Creating multi-span issues in the web UI, by selecting several ranges or adding a range to an
  existing issue. Until then the web UI creates single-span issues, and Claude creates
  multi-span ones.
- A rich pop-over with the issue's text for span markers; they show the number and title as a
  tooltip for now.
- Caching issue placements across requests (§10.2), if placement cost shows up in benchmarks.
  Placement depends only on the anchor and the target commit, so a key of (issue, target commit)
  never goes stale.
- Keeping the session cookie away from other loopback ports, for shared machines (§15). Two
  candidates; neither is tested:
  - A per-server secret hostname (`http://lani-<random>.localhost:<port>/`) with a host-only
    cookie. This depends on the browser resolving `*.localhost` to loopback, which is unconfirmed
    for Safari.
  - A per-server secret path prefix (`/s/<random>/…`) with the cookie's `Path` set to it. This
    should work in every browser, because a hostile page cannot request a path it does not know.
    The cost is a secret segment in every LANI URL.
- Relocating a repo (re-pointing `git_dir` after the repo moves on disk).
- Keeping reviewed commits reachable through `refs/lani/*`. This writes to the repo, so it would
  need an explicit opt-in.
- A Claude Code plugin with slash commands (`/lani-review`, `/lani-fix`).
- CI, prebuilt release binaries, a Homebrew formula, and macOS code signing.

---

## 22. Decisions and open questions

### Decisions (2026-10-07)

| #   | Topic                          | Decision |
| --- | ------------------------------ | -------- |
| D1  | Implementation language        | Rust (§5). |
| D2  | Drafts                         | None. Everything posts immediately, all text stays editable, and authorship is always visible (§11.1). |
| D3  | Uncommitted changes            | Not reviewable. They are detected and reported, and anchors they could affect are rejected (§10.1). |
| D4  | Database location              | One global DB. The web UI lists open reviews across all repos (§7.1, §13.3). |
| D5  | Resolving issues               | The reviewer and you only. The implementer is blocked by the service layer, a DB constraint and the session role lock (§11). |
| D6  | Review types and bases         | Diff reviews against an auto-detected parent or an explicit base; whole-tree reviews with no base, which is the automatic choice on the default branch. The base can be changed in the web UI (§9). |
| D7  | Web server lifetime            | 3 h idle timeout by default, set in the web UI (saved) or with `--idle-timeout` (one run); `never` is allowed (§13.1). |
| D8  | Titles, anchors and threads    | Reviews and issues have titles; issues also have a severity (`high`, `medium`, `low`, `nit`), revised 2026-10-08. Every issue anchors to lines. Comments are linear. The summary has its own comment thread (§4). |
| D9  | Anchor commit                  | Claude passes the commit it read plus its worktree path, and LANI verifies the file on disk matches (§10.1). |
| D10 | Branch with a closed review    | `start_review` creates a new review; closed reviews remain as history (§4). |
| D11 | Role enforcement               | The per-session role lock is enough. Separate per-role MCP configurations were considered and rejected because they make setup harder (§11.3). |

### Open questions

None at present.
