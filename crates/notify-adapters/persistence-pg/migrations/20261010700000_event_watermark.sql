-- The event log orders by transaction rather than by insert (ADR-0028).
--
-- `events.seq` used to come from a sequence, which hands numbers out in insert
-- order, so one global advisory lock held from the insert to the commit was all
-- that stopped a reader passing seq 11 before a slower transaction committed 10.
-- That lock serialised every event-producing write of the deployment.
--
-- Now a row's seq is derived from the id of the transaction that writes it:
--
--     seq = events_seq_base() + xid * 65536 + n      (n = 0, 1, … within the transaction)
--
-- so seq order *is* transaction-id order, and "every transaction older than the
-- oldest one still running has finished" — `pg_snapshot_xmin` — becomes a bound
-- on seq: nothing at or below `events_watermark()` can still appear. Readers stop
-- there, and writers no longer wait on each other.
--
-- Expand-only. The old sequence stays (it still owns the column's history), the
-- column keeps its type, and a pre-ADR-0028 binary still inserts through the
-- default below — it just gets a seq from the new scheme.

-- No writer may draw from the old sequence between reading its high-water mark and
-- switching the default. Readers carry on: EXCLUSIVE blocks writes only.
LOCK TABLE events IN EXCLUSIVE MODE;

-- Every new seq lands above every seq the sequence ever handed out, so a cursor a
-- client already holds still means "after that" — and the head never moves back.
-- Baked into a function rather than kept in a table: it never changes, and an
-- IMMUTABLE constant folds into every query that uses it.
DO $$
DECLARE
    base BIGINT;
BEGIN
    SELECT GREATEST(
               COALESCE((SELECT max(seq) FROM events), 0),
               COALESCE(pg_sequence_last_value('events_seq_seq'::regclass), 0)
           ) + 1
      INTO base;
    EXECUTE format(
        'CREATE FUNCTION events_seq_base() RETURNS BIGINT
             LANGUAGE sql IMMUTABLE PARALLEL SAFE
             AS %L',
        'SELECT ' || base || '::bigint'
    );
END
$$;

-- The next seq for the calling transaction. 65 536 per transaction: the largest
-- batch that appends in one transaction is the purge's (1 000 by default), and the
-- multiplier leaves room for 2^47 transaction ids before a bigint runs out —
-- millennia at a thousand writes a second. Multiplication rather than a shift on
-- purpose: it raises on overflow instead of wrapping.
--
-- `max(seq)` over the transaction's own range sees its own earlier rows (each
-- statement does) and nobody else's (no other transaction has this id), so it is
-- unique without a lock. One row per INSERT statement, which is all this crate
-- ever writes.
CREATE FUNCTION events_next_seq() RETURNS BIGINT
    LANGUAGE plpgsql VOLATILE
AS $$
DECLARE
    lowest BIGINT := events_seq_base() + pg_current_xact_id()::text::bigint * 65536;
    last   BIGINT;
BEGIN
    SELECT max(seq) INTO last FROM events WHERE seq >= lowest AND seq < lowest + 65536;
    IF last = lowest + 65535 THEN
        RAISE EXCEPTION 'more than 65536 events in one transaction'
            USING ERRCODE = 'program_limit_exceeded';
    END IF;
    RETURN COALESCE(last + 1, lowest);
END
$$;

-- The highest seq a reader may serve or a client may be told is "now": every
-- transaction whose id is below the snapshot's xmin has committed or aborted, and
-- every transaction still running — or yet to start — writes above this. STABLE,
-- so it is read off the calling statement's own snapshot: the bound and the rows
-- it bounds are seen at the same instant.
CREATE FUNCTION events_watermark() RETURNS BIGINT
    LANGUAGE sql STABLE
AS $$
    SELECT events_seq_base() + pg_snapshot_xmin(pg_current_snapshot())::text::bigint * 65536 - 1
$$;

ALTER TABLE events ALTER COLUMN seq SET DEFAULT events_next_seq();

-- Put the base back above the log if the transaction counter ever went backwards.
--
-- It can: a *logical* restore (`pg_dump` into a fresh cluster) brings the rows and
-- this function's constant back, but the new cluster numbers its transactions from
-- a few thousand. Every new seq would then sort below the restored ones — below
-- every cursor clients already hold — and the watermark would sit below the whole
-- log. Physical backups, replicas and `pg_upgrade` keep the counter and never
-- trip this. `PgStore::migrate` calls it on every run, which is step 4 of the
-- restore procedure.
--
-- The test is exact, not a heuristic: a committed row's transaction id is below
-- the snapshot's xmax (one past the latest completed transaction), so a visible
-- row whose seq says otherwise can only come from another counter.
CREATE FUNCTION events_rebase() RETURNS BOOLEAN
    LANGUAGE plpgsql VOLATILE
AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM events
        WHERE seq >= events_seq_base()
                     + pg_snapshot_xmax(pg_current_snapshot())::text::bigint * 65536
    ) THEN
        RETURN false;
    END IF;

    -- No writer may hold an uncommitted row while the constant moves. Then every
    -- transaction that can still append has an id at or above xmin, and putting
    -- xmin's first seq just past the newest row puts all of theirs above it too.
    LOCK TABLE events IN EXCLUSIVE MODE;
    EXECUTE format(
        'CREATE OR REPLACE FUNCTION events_seq_base() RETURNS BIGINT
             LANGUAGE sql IMMUTABLE PARALLEL SAFE
             AS %L',
        'SELECT ' || (
            (SELECT max(seq) FROM events) + 1
            - pg_snapshot_xmin(pg_current_snapshot())::text::bigint * 65536
        ) || '::bigint'
    );
    RETURN true;
END
$$;
