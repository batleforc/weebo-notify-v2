-- The stale-closure sweep's "last activity" probe (design §7.2bis).
--
-- `close_stale` asks, per open notification, for `MAX(received_at)` over its
-- revisions. With only `(notification_id, seq)` and `(notification_id,
-- cursor_seq)` to go on, that reads every revision of the notification; with this
-- it is one index probe from the top. Interactions already have the matching
-- `(notification_id, recorded_at)` index.
--
-- Plain CREATE INDEX for the reason the previous migration gives; the same
-- pre-build-by-hand advice applies.
CREATE INDEX IF NOT EXISTS revisions_received_idx
    ON revisions (notification_id, received_at);
