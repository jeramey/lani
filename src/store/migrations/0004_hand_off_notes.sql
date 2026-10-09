-- Schema version 4: every hand-off note is kept. See docs/design.md §11.5.
--
-- The note given with a change of turn, on its turn_changed event, so that earlier notes stay
-- readable after the next hand-off replaces review.turn_note.
ALTER TABLE event ADD COLUMN note TEXT CHECK (kind = 'turn_changed' OR note IS NULL);

-- Until now only the latest note was kept, in review.turn_note. It belongs to each review's
-- latest turn_changed event, because only a change of turn sets it.
UPDATE event SET note = (SELECT turn_note FROM review WHERE review.id = event.review_id)
WHERE kind = 'turn_changed'
  AND id = (SELECT max(later.id) FROM event AS later
            WHERE later.review_id = event.review_id AND later.kind = 'turn_changed');
