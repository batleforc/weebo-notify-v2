-- What the object store was handed, so it can be swept (design §7.2bis, §6.1).
--
-- `POST /attachments/presign` signs a URL and returns an id. If the client
-- uploads and then never publishes, nothing in this database has ever seen that
-- id: no query can name it, and the object is orphaned from birth. That is the
-- half M4novodecies could not reach — it collects attachments whose *notification*
-- expired, which needs a revision to have referenced them at least once (D25).
--
-- A row here means "we signed a URL for this id", not "an object exists". The
-- distinction is safe in the direction that matters: deleting a key that was never
-- written succeeds on S3, so a sweep over rows whose upload never happened is a
-- no-op rather than an error. It is worth knowing when reading the counters.
--
-- Avatars are deliberately absent. They do not go through a presign — the server
-- writes those bytes itself (§2.4) — and their id lives on the profile row, which
-- is what keeps them from being swept as unreferenced.
CREATE TABLE attachment_presigns (
    id           TEXT PRIMARY KEY,
    -- The channel whose `publish` scope authorised the signature (§6.1). Kept for
    -- the operator reading a growing bucket: "who asked for these" is the first
    -- question, and the attachment itself is not scoped to the channel.
    channel_id   TEXT        NOT NULL,
    requested_at TIMESTAMPTZ NOT NULL
);

-- The sweep reads by age and never by id, so this is the index it needs.
CREATE INDEX attachment_presigns_requested_at_idx ON attachment_presigns (requested_at);

-- Backfill, so an instance that already has data does not carry a permanently
-- uncollectable generation of objects. Only what a revision references can be
-- recovered — an upload that was never referenced before this migration is exactly
-- what nothing recorded, and it stays lost to the sweep. `requested_at` is taken
-- from the revision that named it, which is later than the real signature and
-- therefore errs towards keeping the object rather than collecting it early.
INSERT INTO attachment_presigns (id, channel_id, requested_at)
SELECT DISTINCT ON (r.content->'image'->>'ref')
       r.content->'image'->>'ref',
       n.channel_id,
       r.received_at
FROM revisions r
JOIN notifications n ON n.id = r.notification_id
WHERE r.content->'image'->>'kind' = 'attachment'
  AND r.content->'image'->>'ref' IS NOT NULL
ORDER BY r.content->'image'->>'ref', r.received_at
ON CONFLICT (id) DO NOTHING;
