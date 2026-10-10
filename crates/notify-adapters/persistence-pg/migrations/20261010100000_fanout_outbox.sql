-- Device push and thumbnail work a committed publish still owes (ADR-0025).
--
-- Written in the publish's own transaction, so a request dropped after the commit
-- (a client giving up, a SIGTERM drain running out) cannot lose it: a worker on any
-- replica leases rows with `FOR UPDATE SKIP LOCKED`, does the work, deletes them.
CREATE TABLE fanout_outbox (
    -- The cursor the write landed on: one write, one row. No foreign key on
    -- `events`, which the purge prunes on its own schedule.
    cursor_seq   BIGINT PRIMARY KEY,
    -- The notification as the write left it: what the push carries.
    notification JSONB NOT NULL,
    -- The attachment whose thumbnail to derive (design D30), if any.
    thumbnail    TEXT,
    -- Held by a worker until this instant; NULL is nobody's. A lease, not a
    -- delete-on-claim, so a worker killed mid-push leaves the row owed.
    leased_until TIMESTAMPTZ,
    queued_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
