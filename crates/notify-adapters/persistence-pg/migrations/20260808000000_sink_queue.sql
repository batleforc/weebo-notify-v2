-- Outbound sink deliveries and their dead letters (design §7.1bis, scenario T17).
--
-- Two tables rather than one with a flag: a delivery that is owed and a delivery
-- that was abandoned are read by different things at different rates — a drain
-- claiming due rows every few seconds, an admin console listing failures — and one
-- table would make the hot query step over the cold rows for ever.
--
-- What is deliberately *not* here: the sink's target, and therefore its HMAC
-- secret. A row names its sink (`kind:destination`), and the target is resolved
-- from the configuration at send time (§11). That keeps the shared secret out of
-- every dump and backup, makes a rotation apply to what is already queued, and
-- makes a sink removed from the configuration stop being retried rather than being
-- retried at a place nobody serves.
CREATE TABLE sink_queue (
    delivery_id     TEXT PRIMARY KEY,
    channel_id      TEXT NOT NULL,
    -- `webhook:https://…` or `email:…`, as design §7.1bis derives it.
    sink            TEXT NOT NULL,
    -- The stream event, verbatim (ADR-0013): what a consumer receives is what a
    -- client would have seen on its stream, byte for byte.
    event           JSONB NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    -- The give-up clock (24 h), which starts at the first failure and not at the
    -- publish: a consumer that goes down hours after a busy publish gets its full
    -- day from the moment the trouble started.
    first_failed_at TIMESTAMPTZ,
    -- When the next attempt is allowed.
    due_at          TIMESTAMPTZ NOT NULL,
    -- Held by a drain until this instant; NULL is nobody's.
    --
    -- A lease and not a delete-on-claim: a leader killed mid-attempt must leave the
    -- delivery owed rather than lose it, which is the difference between an
    -- at-least-once promise and one that quietly drops on a restart.
    leased_until    TIMESTAMPTZ,
    queued_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The drain's only query: due, unleased, oldest first.
CREATE INDEX sink_queue_due_idx ON sink_queue (due_at, leased_until);

CREATE TABLE sink_dead_letters (
    delivery_id     TEXT PRIMARY KEY,
    channel_id      TEXT NOT NULL,
    sink            TEXT NOT NULL,
    -- Kept so a requeue puts back something a drain can actually send. Without it
    -- `POST /admin/dlq/{id}/requeue` would only be able to forget the failure.
    event           JSONB NOT NULL,
    attempts        INTEGER NOT NULL,
    last_error      TEXT NOT NULL,
    first_failed_at TIMESTAMPTZ NOT NULL,
    dead_at         TIMESTAMPTZ NOT NULL
);

-- The console lists per channel, and `sink_dlq_depth` (design §13) groups by it.
CREATE INDEX sink_dead_letters_channel_idx ON sink_dead_letters (channel_id, dead_at DESC);
