-- Audit retention through one sanctioned purge (ADR-0027).
--
-- `20261010300000_redact_audit_webhook_urls.sql` made the append-only rule a
-- trigger that refuses every `UPDATE`, `DELETE` and `TRUNCATE`. This keeps that,
-- and opens exactly one door: `wnk_audit_purge(cutoff, batch)`, which deletes a
-- bounded batch of entries older than `cutoff` and records that it did.
--
-- How the door is held:
--
--   * The function refuses a cutoff younger than 90 days. Hard-coded here, not
--     read from configuration: a caller holding the application's credentials is
--     exactly who this must not trust.
--   * It announces itself to the trigger with a transaction-local setting whose
--     value is the cutoff, and clears it before returning. The trigger lets a
--     `DELETE` through only while that setting is present, and only for a row
--     older than both the cutoff and `now() - 90 days` — the second test is the
--     floor again, so somebody who sets `wnk.audit_purge` by hand still cannot
--     reach a recent row.
--   * Every batch that deleted anything writes an `audit.purged` entry in the same
--     transaction. A purge nobody configured still leaves its mark.
--   * `UPDATE` and `TRUNCATE` stay refused, setting or not.
--
-- What it does not do: stop a role that owns the table. That role can disable the
-- trigger, as the redaction migration does, and nothing in the database can
-- prevent it. The guarantee that survives that is a copy of the log somewhere the
-- database's credentials cannot write (ADR-0027, "What this does not buy").
--
-- Idempotent (`CREATE OR REPLACE` throughout), and it has to be: the conformance
-- suite re-runs the redaction migration, whose own `CREATE OR REPLACE` puts the
-- refuse-everything trigger function back, and then re-runs this to restore the
-- door. The trigger names and the function the triggers call are unchanged, so the
-- redaction's `ALTER TABLE ... DISABLE TRIGGER audit_entries_append_only` keeps
-- naming the right trigger.

CREATE OR REPLACE FUNCTION audit_entries_append_only() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
        DECLARE
            purge_cutoff TEXT;
        BEGIN
            -- Nested rather than one `AND`: the `TRUNCATE` trigger is per
            -- statement and has no `OLD` to read.
            IF TG_OP = 'DELETE' THEN
                purge_cutoff := nullif(current_setting('wnk.audit_purge', true), '');
                IF purge_cutoff IS NOT NULL
                   AND OLD.at < least(purge_cutoff::timestamptz, now() - interval '90 days') THEN
                    RETURN OLD;
                END IF;
            END IF;

            RAISE EXCEPTION 'audit_entries is append-only (SEC-P2-37, ADR-0026, ADR-0027): % refused', TG_OP
                USING ERRCODE = 'insufficient_privilege';
        END
    $$;

-- `SECURITY DEFINER` so a deployment that splits roles can grant the application
-- `EXECUTE` on this and nothing else on the table; `search_path` pinned to the one
-- the migration ran under, so a caller cannot shadow `audit_entries` with a table
-- of their own.
CREATE OR REPLACE FUNCTION wnk_audit_purge(cutoff TIMESTAMPTZ, batch INTEGER) RETURNS BIGINT
    LANGUAGE plpgsql
    SECURITY DEFINER
    SET search_path FROM CURRENT
    AS $$
        DECLARE
            purged BIGINT;
        BEGIN
            IF cutoff IS NULL OR cutoff > now() - interval '90 days' THEN
                RAISE EXCEPTION 'wnk_audit_purge: cutoff % is inside the 90-day floor (ADR-0027)', cutoff
                    USING ERRCODE = 'invalid_parameter_value';
            END IF;
            IF batch IS NULL OR batch < 1 THEN
                RAISE EXCEPTION 'wnk_audit_purge: batch must be positive, got %', batch
                    USING ERRCODE = 'invalid_parameter_value';
            END IF;

            PERFORM set_config('wnk.audit_purge', cutoff::text, true);

            -- Oldest first on `audit_at_idx`, so a backlog drains from the far end
            -- and a pass cut short by its budget never leaves a hole in the middle.
            WITH doomed AS (
                SELECT seq FROM audit_entries
                 WHERE at < cutoff
                 ORDER BY at, seq
                 LIMIT batch
            ), gone AS (
                DELETE FROM audit_entries AS entry
                 USING doomed
                 WHERE entry.seq = doomed.seq
             RETURNING 1
            )
            SELECT count(*) INTO purged FROM gone;

            -- Closed again before anything else in the caller's transaction runs:
            -- the setting is the door, and it is only open for the statement above.
            PERFORM set_config('wnk.audit_purge', '', true);

            IF purged > 0 THEN
                INSERT INTO audit_entries (at, actor_ref, actor, action, detail)
                VALUES (now(),
                        'configuration',
                        '{"kind":"configuration"}'::jsonb,
                        'audit.purged',
                        jsonb_build_object(
                            'purged_entries', purged,
                            'cutoff', cutoff,
                            'adr', '0027'));
            END IF;

            RETURN purged;
        END
    $$;

-- Owner only by default. A deployment whose application role is not the
-- migration role grants it explicitly, which is the deliberate act it should be.
REVOKE ALL ON FUNCTION wnk_audit_purge(TIMESTAMPTZ, INTEGER) FROM PUBLIC;
