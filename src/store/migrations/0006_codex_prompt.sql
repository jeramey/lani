-- Schema version 6: a Codex reviewer prompt alongside Claude Code and other harnesses (§13.9).
-- SQLite cannot alter the kind CHECK in place. Nothing references prompt_edit, so rebuild it
-- with the additional kind, keeping every revision and its id.
CREATE TABLE prompt_edit_rebuilt (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  review_id    INTEGER NOT NULL REFERENCES review(id),
  kind         TEXT NOT NULL CHECK (kind IN ('claude_code', 'codex', 'generic')),
  default_id   INTEGER NOT NULL REFERENCES prompt_default(id),
  patch        TEXT,
  lani_version TEXT NOT NULL,
  created_at   TEXT NOT NULL
) STRICT;
INSERT INTO prompt_edit_rebuilt (id, review_id, kind, default_id, patch, lani_version, created_at)
  SELECT id, review_id, kind, default_id, patch, lani_version, created_at FROM prompt_edit;
DROP TABLE prompt_edit;
ALTER TABLE prompt_edit_rebuilt RENAME TO prompt_edit;
CREATE INDEX prompt_edit_by_review ON prompt_edit(review_id, kind, id);
