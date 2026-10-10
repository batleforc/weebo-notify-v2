-- Webhook sink ids no longer carry the full url (design §7.1bis).
--
-- A sink id used to be `webhook:<url>`, and many consumers (Slack, Discord, Teams)
-- keep their credential in the url's *path*. That id is stored here, listed by the
-- dead-letter API, written into the audit log and logged. It is now
-- `webhook:<origin>#<fingerprint>`, the fingerprint being the first 16 hex digits
-- of HMAC-SHA256 under `[sinks] fingerprint_key` (`notify_ports::redact_url`).
--
-- What this file does NOT rewrite: `sink_queue.sink` and `sink_dead_letters.sink`.
-- SQL never sees the key, so it cannot compute the new id, and a keyless digest
-- here would be exactly the brute-forceable value the key exists to avoid. The
-- server re-keys those rows at boot instead (`notify-server/src/sinks.rs`,
-- `rekey_stored`), knowing both the key and the configured urls; a url no
-- configured webhook matches is redacted there to `<origin>#unrouted`.
--
-- What it does rewrite: the reconciler's last-applied snapshots. They are only ever
-- compared, never routed by, so a keyless marker is enough — `<origin>#redacted` —
-- and without it the first pass after the upgrade would journal a "changed" entry
-- whose `previous` side is the very url this migration exists to stop repeating.
-- That pass still journals each channel with a webhook as changed once (the
-- marker becomes the keyed fingerprint), which is the truth: the stored form moved.
--
-- Only urls still in the old shape are touched (something after the origin that is
-- a path or a query, where the new shapes have `#`), so running it against a
-- database that is already current changes nothing.
UPDATE managed_declarations
   SET snapshot = jsonb_set(
           snapshot,
           '{sinks}',
           (SELECT jsonb_agg(
                       CASE
                           WHEN sink ->> 'type' = 'webhook'
                                AND sink ->> 'url' ~ '^[^:/]+://[^/?#]*[/?]'
                           THEN jsonb_set(
                                    sink,
                                    '{url}',
                                    to_jsonb(substring(sink ->> 'url' FROM '^[^:/]+://[^/?#]*')
                                             || '#redacted'))
                           ELSE sink
                       END
                       ORDER BY position)
              FROM jsonb_array_elements(snapshot -> 'sinks') WITH ORDINALITY AS t(sink, position)))
 WHERE kind = 'channel'
   AND jsonb_typeof(snapshot -> 'sinks') = 'array'
   AND jsonb_array_length(snapshot -> 'sinks') > 0;

-- Dead letters are now pruned once older than `[sinks] dead_letter_retention`; this
-- is the index that delete walks. The depth gauge's `GROUP BY channel_id` is
-- already served by `sink_dead_letters_channel_idx`, and the retention is what keeps
-- the table it scans bounded.
CREATE INDEX IF NOT EXISTS sink_dead_letters_dead_at_idx ON sink_dead_letters (dead_at);
