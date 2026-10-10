-- Schema version 5: the human's edits to the reviewer prompts that the review page copies. See
-- docs/design.md §13.9.

-- Each distinct default text that an edit was made from, stored once.
CREATE TABLE prompt_default (
  id   INTEGER PRIMARY KEY AUTOINCREMENT,
  text TEXT NOT NULL UNIQUE
) STRICT;

-- One revision each time the human copies a prompt whose text differs from what the review page
-- showed for it: the edit as a unified diff from its default, or NULL when the text went back to
-- the default. The latest revision of a review's prompt is what the page shows next.
CREATE TABLE prompt_edit (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  review_id    INTEGER NOT NULL REFERENCES review(id),
  kind         TEXT NOT NULL CHECK (kind IN ('claude_code', 'generic')),
  default_id   INTEGER NOT NULL REFERENCES prompt_default(id),
  patch        TEXT,
  lani_version TEXT NOT NULL,
  created_at   TEXT NOT NULL
) STRICT;
CREATE INDEX prompt_edit_by_review ON prompt_edit(review_id, kind, id);
