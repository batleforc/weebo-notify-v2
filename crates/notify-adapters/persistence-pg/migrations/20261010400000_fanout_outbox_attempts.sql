-- A poison row must not hold the fan-out outbox hostage (ADR-0025).
--
-- Before this, a row that failed to decode failed the whole claim, so the batch it
-- sat in was re-leased and re-failed forever behind it; and a row whose work kept
-- dying (a crash, a timeout) was claimed again every lease, with no end.
ALTER TABLE fanout_outbox
    -- Claims so far. Counted at claim time rather than on failure, so a worker that
    -- dies mid-row (and so never reports anything) still uses up an attempt.
    ADD COLUMN attempts   INTEGER NOT NULL DEFAULT 0,
    -- Why the last attempt did not finish, for whoever reads the dead rows.
    ADD COLUMN last_error TEXT,
    -- Set once the row has used up its attempts, could not be decoded, or got too
    -- old to be worth a push. A dead row is never claimed again; it stays for an
    -- operator to read (`fanout_outbox_rows{kind="dead"}` counts them) and is
    -- deleted by hand.
    ADD COLUMN dead_at    TIMESTAMPTZ;

-- The claim reads live rows oldest first; dead ones would otherwise be scanned
-- past on every round.
CREATE INDEX fanout_outbox_claimable_idx ON fanout_outbox (cursor_seq) WHERE dead_at IS NULL;

-- Deleting or expiring a notification drops what it still owes, by the id inside
-- the stored snapshot: no new column, so a replica still running the previous
-- release keeps inserting rows this finds.
CREATE INDEX fanout_outbox_notification_idx ON fanout_outbox ((notification ->> 'id'));
