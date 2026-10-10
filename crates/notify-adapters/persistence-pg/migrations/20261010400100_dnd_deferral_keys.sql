-- Which held-back pushes a quiet period's tally has already counted (design §7.4).
--
-- The fan-out outbox is at least once (ADR-0025): a row retried after a timeout or
-- a lease that ran out pushes again, and a user with three devices is asked about
-- three times. Each of those used to add one to the digest's count, so "4
-- notifications while you were away" could mean one. A key per (user, revision)
-- makes counting idempotent: the tally only moves when the key is new.
--
-- The trade the dnd_digest migration declined — a row per held-back push — taken
-- because a wrong number is worse than a small table: rows only exist for users who
-- are quiet right now, and are dropped whenever their tally is cleared or their
-- preference replaced.
CREATE TABLE dnd_deferrals (
    user_id         TEXT    NOT NULL REFERENCES user_dnd (user_id) ON DELETE CASCADE,
    notification_id TEXT    NOT NULL,
    seq             INTEGER NOT NULL,
    PRIMARY KEY (user_id, notification_id, seq)
);
