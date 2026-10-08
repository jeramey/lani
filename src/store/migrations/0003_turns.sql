-- Schema version 3: turns, and questions for the human. See docs/design.md §11.5.
--
-- Columns added to existing tables need defaults or NULL for the rows already there; the service
-- still writes every column explicitly.

-- Whose turn a review is: the role that should act now, or NULL for nobody. turn_by and turn_at
-- record the last change of turn, turn_note the note given with it. rounds counts the times the
-- implementer handed the review back to the reviewer since the human last handed it on.
ALTER TABLE review ADD COLUMN turn TEXT CHECK (turn IN ('reviewer', 'implementer', 'human'));
ALTER TABLE review ADD COLUMN turn_by TEXT CHECK (turn_by IN ('human', 'reviewer', 'implementer'));
ALTER TABLE review ADD COLUMN turn_at TEXT CHECK ((turn_by IS NULL) = (turn_at IS NULL));
ALTER TABLE review ADD COLUMN turn_note TEXT;
ALTER TABLE review ADD COLUMN rounds INTEGER NOT NULL DEFAULT 0 CHECK (rounds >= 0);

-- An issue a Claude session asked the human about. The human commenting on it, resolving it or
-- reopening it clears the question.
ALTER TABLE issue ADD COLUMN asked_by TEXT CHECK (asked_by IN ('reviewer', 'implementer'));
ALTER TABLE issue ADD COLUMN asked_at TEXT CHECK ((asked_by IS NULL) = (asked_at IS NULL));

-- The event table gains two kinds, and turn_to for the new holder of a turn. A CHECK constraint
-- cannot be altered in place, so the table is rebuilt; nothing references it.
CREATE TABLE event_rebuilt (
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
INSERT INTO event_rebuilt (id, review_id, issue_id, comment_id, kind, actor, turn_to, created_at)
  SELECT id, review_id, issue_id, comment_id, kind, actor, NULL, created_at FROM event;
DROP TABLE event;
ALTER TABLE event_rebuilt RENAME TO event;
CREATE INDEX event_by_review ON event(review_id, id);

-- How many rounds the reviewer and implementer may take before the review goes to the human.
INSERT INTO setting (key, value) VALUES ('round_limit', '5');
