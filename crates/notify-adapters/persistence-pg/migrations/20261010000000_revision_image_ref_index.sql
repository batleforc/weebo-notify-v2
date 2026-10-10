-- The attachment lookups by id (design §6.1, §7.2bis, D25).
--
-- Three queries ask "which revisions name this attachment": the download check
-- (`attachment_is_visible`), the orphan sweep (`orphaned_attachments`) and the
-- unreferenced-presign sweep (`unreferenced`). Without this each of them is a
-- sequential scan of `revisions` — per request for the first, per candidate row
-- for the other two — which is invisible on a dev database and the slowest thing
-- on the instance once the table holds a year of history.
--
-- Partial on exactly the predicate every one of those queries carries
-- (`kind = 'attachment'`), so revisions without an image — the overwhelming
-- majority — are not in it at all.
--
-- Plain CREATE INDEX, not CONCURRENTLY, and that is deliberate. sqlx serialises
-- migrators with a session advisory lock, and a replica waiting on that lock sits
-- in a statement holding a snapshot — which CONCURRENTLY waits out while holding
-- the lock it is waiting for. Replicas booting together deadlock (seen in the
-- conformance suite: "deadlock detected ... SELECT pg_advisory_lock"), and a
-- failed CONCURRENTLY build leaves an INVALID index that IF NOT EXISTS would then
-- skip for ever. The price is that `revisions` refuses writes for the length of
-- the build. On a large instance, build it by hand first with
-- `CREATE INDEX CONCURRENTLY IF NOT EXISTS ...` (same definition) and this
-- migration becomes a no-op.
CREATE INDEX IF NOT EXISTS revisions_image_ref_idx
    ON revisions ((content->'image'->>'ref'))
    WHERE content->'image'->>'kind' = 'attachment';
