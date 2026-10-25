-- The two counts of every /sync page (events.rs): without these, `expired_while_away`
-- walked the whole log for a kind it rarely finds, and `compacted` read every row of the
-- window from the heap. Both are now index-only scans over their own kind.
-- Plain CREATE INDEX, as in 20261010000000_revision_image_ref_index.sql (CONCURRENTLY
-- deadlocks replicas migrating together); on a large instance build them CONCURRENTLY by hand
-- first, and IF NOT EXISTS makes this a no-op.
CREATE INDEX IF NOT EXISTS events_expired_unseen_idx ON events (channel_id, seq) WHERE kind = 'expired_unseen';
CREATE INDEX IF NOT EXISTS events_revision_idx ON events (channel_id, seq) INCLUDE (notification_id)
    WHERE kind = 'revision';
