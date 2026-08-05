-- What a quiet period held back, so its end can be one push (design §7.4, M3.4).
--
-- **Two counters, not a table of held-back notifications.** The digest says
-- "12 notifications while you were away (2 failure)", so two integers answer it
-- exactly. A row per suppressed push would grow on the fan-out path — the hottest
-- path in the product — to be read once and thrown away, and the only thing it would
-- buy is the ability to *list* them, which no screen asks for: the inbox already
-- holds them.
--
-- Failures get a column of their own rather than a per-status tally because they are
-- the one status worth calling out on a lock screen: the difference between "catch
-- up when you can" and "look now". Five columns nobody renders would be five columns
-- to keep in step with an enum.
--
-- On `user_dnd` rather than in a table of its own: a tally without a preference is
-- meaningless — somebody with no preference cannot be muted, so nothing can be held
-- back for them — and replacing a preference has to clear the tally in the same
-- statement, which is free when they share a row.
ALTER TABLE user_dnd
    ADD COLUMN deferred_total    INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN deferred_failures INTEGER NOT NULL DEFAULT 0;

-- The digest job scans for anything held back, and on a healthy instance almost
-- nobody is quiet at any given moment.
CREATE INDEX user_dnd_deferred_idx ON user_dnd (user_id) WHERE deferred_total > 0;
