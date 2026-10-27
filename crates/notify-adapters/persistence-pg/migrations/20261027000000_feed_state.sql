-- What the feed poller knows of each declared feed (RFC 0002 §6.4). Keyed on an
-- unkeyed digest of channel and URL: the URL itself, which may carry a token, is
-- never stored. `batch` holds the chosen notifications whole (decision 8), so a
-- replay publishes exactly what was chosen; a summary is one of its entries.
CREATE TABLE feed_state (
    feed_key       text        PRIMARY KEY,
    channel_id     text        NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    -- NULL until the first poll; at most 400 item keys, in first-sight order.
    seen           text[],
    batch          jsonb       NOT NULL DEFAULT '[]',
    -- Bumped by every save; a save from a stale copy matches no row (decision 11).
    version        bigint      NOT NULL DEFAULT 0,
    etag           text,
    last_modified  text,
    failures       integer     NOT NULL DEFAULT 0,
    next_poll_at   timestamptz NOT NULL,
    last_ok_at     timestamptz,
    -- The last pass that saw this feed declared (decision 9).
    touched_at     timestamptz NOT NULL
);

CREATE INDEX feed_state_due ON feed_state (next_poll_at);
