-- The index behind the stale-closure sweep (design §7.2bis).
--
-- Index only, no column: `close_after` lives in the channel policy, which is
-- stored as JSON, so nothing about this feature needs a shape change. What it
-- does need is a way to find open notifications without reading every row a
-- channel ever published — the sweep runs on a timer, forever, and a sequential
-- scan of `notifications` per tick is the kind of cost that is invisible in
-- development and decides an incident later.
--
-- Partial on exactly the sweep's predicate. `closed` rows are the overwhelming
-- majority in a healthy instance and are excluded from the index entirely, so it
-- stays roughly the size of the backlog that is actually open rather than the
-- size of the table.
--
-- `channel_id` leads because the sweep joins on it: the deadline is per channel,
-- so the query looks up "this channel's open notifications" once per configured
-- channel rather than filtering a global list.
CREATE INDEX IF NOT EXISTS notifications_open_idx
    ON notifications (channel_id, created_at)
    WHERE deleted_at IS NULL AND state <> 'closed';
