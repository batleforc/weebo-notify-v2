-- A one-time redaction of webhook urls in the audit log (ADR-0026).
--
-- Before `20261010200000_redacted_sink_ids.sql`, the reconciler journalled a
-- managed channel's declaration as it was written, `sinks[].url` included, into
-- `detail.declaration` and `detail.previous` — and Slack, Discord and Teams keep
-- their credential in that url's path. That migration (and the server's boot-time
-- re-key) dealt with every other copy and left these, because the audit log is
-- evidence. ADR-0026 is the decision that a
-- secret is not evidence: this file is the only sanctioned mutation of the table,
-- and it is written to stay the only one.
--
-- Three things, all in the migration's own transaction:
--
--   1. The append-only rule, which was a convention (SEC-P2-37) and a source scan
--      (`tests/audit_is_append_only.rs`), becomes a trigger: `UPDATE`, `DELETE` and
--      `TRUNCATE` on `audit_entries` are refused for every role that does not first
--      disable it — which takes ownership and leaves a visible statement behind.
--   2. The redaction itself, with that trigger off for the length of one statement.
--      `ALTER TABLE` is transactional and takes an ACCESS EXCLUSIVE lock, so no
--      other session ever sees the table unguarded: it waits, then finds it guarded
--      again.
--   3. An entry saying it happened and how many rows it touched — an edit to the
--      log that the log does not record would be the thing it exists to prevent.
--
-- Idempotent: the redacted shapes (`<origin>#redacted`, and the `<origin>#<hmac>`
-- the reconciler writes now) have no path or query after the origin, so a second run matches nothing, writes no entry, and the
-- `CREATE OR REPLACE`s leave the trigger as they found it. The conformance suite
-- relies on that: it runs this file again against a planted old-format entry.

CREATE OR REPLACE FUNCTION audit_entries_append_only() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
        BEGIN
            RAISE EXCEPTION 'audit_entries is append-only (SEC-P2-37, ADR-0026): % refused', TG_OP
                USING ERRCODE = 'insufficient_privilege';
        END
    $$;

CREATE OR REPLACE TRIGGER audit_entries_append_only
    BEFORE UPDATE OR DELETE ON audit_entries
    FOR EACH ROW EXECUTE FUNCTION audit_entries_append_only();

-- Row triggers do not fire on TRUNCATE, which is the bulk form of the same act.
CREATE OR REPLACE TRIGGER audit_entries_no_truncate
    BEFORE TRUNCATE ON audit_entries
    FOR EACH STATEMENT EXECUTE FUNCTION audit_entries_append_only();

-- Origin and a keyless marker: `https://hooks.slack.com#redacted`. Not the sink
-- id's fingerprint — that is an HMAC under `[sinks] fingerprint_key`, which SQL
-- never sees — and an audit entry needs none: it records that a declaration
-- changed, not where to deliver. Anything keyless that kept the url apart from its
-- neighbours (a plain digest) could be brute-forced back to it, so nothing does.
CREATE OR REPLACE FUNCTION pg_temp.wnk_redact(url TEXT) RETURNS TEXT
    LANGUAGE sql IMMUTABLE
    AS $$
        SELECT substring(url FROM '^[^:/]+://[^/?#]*') || '#redacted'
    $$;

-- A declaration's `sinks` array with its old-shape webhook urls redacted, the way
-- the previous migration rewrote `managed_declarations.snapshot`. Anything that is
-- not a non-empty array comes back untouched.
CREATE OR REPLACE FUNCTION pg_temp.wnk_redact_sinks(sinks JSONB) RETURNS JSONB
    LANGUAGE sql IMMUTABLE
    AS $$
        SELECT coalesce(
            (SELECT jsonb_agg(
                        CASE
                            WHEN sink ->> 'type' = 'webhook'
                                 AND sink ->> 'url' ~ '^[^:/]+://[^/?#]*[/?]'
                            THEN jsonb_set(sink, '{url}', to_jsonb(pg_temp.wnk_redact(sink ->> 'url')))
                            ELSE sink
                        END
                        ORDER BY position)
               FROM jsonb_array_elements(
                        CASE WHEN jsonb_typeof(sinks) = 'array' THEN sinks ELSE '[]'::jsonb END)
                    WITH ORDINALITY AS t(sink, position)),
            sinks)
    $$;

-- `detail.declaration` and `detail.previous` are the only places the reconciler
-- (`notify-server/src/reconcile.rs`, `record`) ever put a declaration.
CREATE OR REPLACE FUNCTION pg_temp.wnk_redact_detail(detail JSONB) RETURNS JSONB
    LANGUAGE sql IMMUTABLE
    AS $$
        SELECT jsonb_object_agg(
                   key,
                   CASE
                       WHEN key IN ('declaration', 'previous')
                            AND jsonb_typeof(value) = 'object'
                            AND value ? 'sinks'
                       THEN jsonb_set(value, '{sinks}', pg_temp.wnk_redact_sinks(value -> 'sinks'))
                       ELSE value
                   END)
          FROM jsonb_each(detail)
    $$;

ALTER TABLE audit_entries DISABLE TRIGGER audit_entries_append_only;

-- The actor is the configuration: the closed `AuditActor` set has nothing closer
-- for "the operator deployed this build", and a fifth kind for one row would be a
-- variant every reader has to learn for nothing.
WITH redacted AS (
    UPDATE audit_entries AS entry
       SET detail = rewritten.detail
      FROM (SELECT seq, pg_temp.wnk_redact_detail(detail) AS detail
              FROM audit_entries
             WHERE jsonb_typeof(detail) = 'object'
               AND detail ?| ARRAY['declaration', 'previous']) AS rewritten
     WHERE entry.seq = rewritten.seq
       AND entry.detail IS DISTINCT FROM rewritten.detail
 RETURNING entry.seq
)
INSERT INTO audit_entries (at, actor_ref, actor, action, detail)
SELECT now(),
       'configuration',
       '{"kind":"configuration"}'::jsonb,
       'audit.redacted',
       jsonb_build_object(
           'redacted_entries', count(*),
           'reason', 'webhook urls carried credentials in their path',
           'adr', '0026',
           'migration', '20261010300000')
  FROM redacted
HAVING count(*) > 0;

ALTER TABLE audit_entries ENABLE TRIGGER audit_entries_append_only;
