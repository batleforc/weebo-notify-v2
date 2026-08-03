-- The initial schema (design §4, ADR-0012).
--
-- Two rules run through all of it.
--
-- **Columns for what is queried, jsonb for what is only ever read back whole.**
-- Filters, orderings and joins get real columns with real indexes; rich payloads
-- (content, actions, policy, authors) are stored in the domain's own serde
-- representation. That representation is already the wire contract (ADR-0013), so
-- this adds no second format to keep in step — and a domain rename shows up as a
-- spec diff in the `contract` job rather than as silent rot.
--
-- **`events.seq` is the cursor.** Every revision, receipt, interaction, tombstone
-- and preference gets a row here, and that bigserial *is* what a client sends back
-- as `?cursor=` or `Last-Event-ID` (ADR-0012). Nothing else may allocate one.
--
-- Identifiers are stored as their prefixed text form (`ntf_01J…`, `ci-infra`)
-- rather than as raw ULIDs: they are self-describing in a query someone runs at
-- 3am, and the domain parses them strictly on the way back in anyway.

-- --------------------------------------------------------------------------
-- Channels
-- --------------------------------------------------------------------------

CREATE TABLE channels (
    id            TEXT PRIMARY KEY,
    display_name  TEXT,
    -- 'managed' (GitOps owns its shape, §11) or 'dynamic' (the API does).
    origin        TEXT        NOT NULL,
    policy        JSONB       NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- This table's pagination cursor. Its own sequence rather than `events.seq`:
    -- creating a channel is not something a client replays, and borrowing the
    -- stream's order would put it in everybody's backlog.
    seq           BIGSERIAL   NOT NULL UNIQUE
);

-- The reconciler asks for exactly this set on boot and on SIGHUP.
CREATE INDEX channels_managed_idx ON channels (id) WHERE origin = 'managed';

CREATE TABLE channel_members (
    channel_id     TEXT  NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    -- Principal::reference() — unique within its kind, and the kinds carry
    -- distinct prefixes, so it is unique full stop.
    principal_ref  TEXT  NOT NULL,
    principal      JSONB NOT NULL,
    scopes         TEXT[] NOT NULL,
    PRIMARY KEY (channel_id, principal_ref)
);

-- "Which channels may this principal see?" is the query behind the inbox, the
-- stream filter and the search scope, so it gets its own index.
CREATE INDEX channel_members_principal_idx ON channel_members (principal_ref);

-- The second door to a channel (§11): access granted to a whole identity-provider
-- group rather than to a named principal.
--
-- Deliberately NOT a row per user. Who is in `infra-admins` is a fact the provider
-- owns and the token carries; materialising it here would be a session by another
-- name, and it would go stale the moment somebody leaves the group. So this table
-- holds the *binding*, and the caller's `groups` claim is joined against it at read
-- time (design §5.1).
CREATE TABLE channel_group_access (
    channel_id  TEXT   NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    oidc_group  TEXT   NOT NULL,
    scopes      TEXT[] NOT NULL,
    PRIMARY KEY (channel_id, oidc_group)
);

CREATE INDEX channel_group_access_group_idx ON channel_group_access (oidc_group);

-- --------------------------------------------------------------------------
-- Identities
-- --------------------------------------------------------------------------

CREATE TABLE users (
    id            TEXT PRIMARY KEY,
    oidc_subject  TEXT        NOT NULL UNIQUE,
    email         TEXT,
    display_name  TEXT,
    created_at    TIMESTAMPTZ NOT NULL
);

-- Individual channel bindings key on the address (§11), and it is the only path
-- an identity provider with no groups claim can offer (T29).
CREATE INDEX users_email_idx ON users (lower(email)) WHERE email IS NOT NULL;

CREATE TABLE service_accounts (
    id            TEXT PRIMARY KEY,
    display_name  TEXT,
    description   TEXT,
    origin        TEXT        NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL,
    disabled_at   TIMESTAMPTZ
);

CREATE TABLE api_keys (
    id            TEXT PRIMARY KEY,
    -- The whole reason the prefix exists: authenticating is one indexed read
    -- rather than an Argon2 comparison against every key on the instance.
    prefix        TEXT        NOT NULL UNIQUE,
    secret_hash   TEXT        NOT NULL,
    label         TEXT,
    owner_ref     TEXT        NOT NULL,
    owner         JSONB       NOT NULL,
    scopes        TEXT[]      NOT NULL,
    -- Empty means "every channel the owner can reach"; a non-empty list narrows.
    channels      TEXT[]      NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL,
    expires_at    TIMESTAMPTZ,
    last_used_at  TIMESTAMPTZ,
    revoked_at    TIMESTAMPTZ
);

CREATE INDEX api_keys_owner_idx ON api_keys (owner_ref);

-- --------------------------------------------------------------------------
-- Devices
-- --------------------------------------------------------------------------

CREATE TABLE devices (
    id            TEXT PRIMARY KEY,
    owner_ref     TEXT        NOT NULL,
    owner         JSONB       NOT NULL,
    label         TEXT        NOT NULL,
    transport     JSONB       NOT NULL,
    -- X25519, absent until a device that needs sealed boxes enrols one (§6.2).
    public_key    BYTEA,
    -- How far this device has read. Monotonic, enforced by the UPDATE itself.
    cursor_seq    BIGINT,
    enrolled_at   TIMESTAMPTZ NOT NULL,
    last_seen_at  TIMESTAMPTZ,
    revoked_at    TIMESTAMPTZ
);

CREATE INDEX devices_owner_idx ON devices (owner_ref);
CREATE INDEX devices_active_idx ON devices (owner_ref) WHERE revoked_at IS NULL;

-- --------------------------------------------------------------------------
-- Notifications and their revisions
-- --------------------------------------------------------------------------

CREATE TABLE notifications (
    id              TEXT PRIMARY KEY,
    channel_id      TEXT        NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    external_id     TEXT,
    -- 'lifecycle' or 'oneshot' (§2.2).
    kind            TEXT        NOT NULL,
    last_seq        INTEGER     NOT NULL,
    revision_count  INTEGER     NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL,
    closed_at       TIMESTAMPTZ,
    deleted_at      TIMESTAMPTZ,
    -- Where this notification's most recent write landed in the event log. The
    -- inbox is ordered by it, so "most recent activity first" is a revision
    -- arriving rather than the row having been created — which is the whole point
    -- of a notification being a lifecycle (§2.2).
    cursor_seq      BIGINT      NOT NULL,
    ttl             JSONB       NOT NULL,
    -- closed_at + ttl, recomputed on every write. NULL means it is either still
    -- open or kept forever — in both cases retention has not started.
    expires_at      TIMESTAMPTZ,

    -- The head revision, denormalised. An inbox filters on state, status,
    -- priority, tags and group_key and orders by id; joining to revisions for
    -- that would make the one hot read the one that needs a join.
    head_seq        INTEGER     NOT NULL,
    state           TEXT        NOT NULL,
    status          TEXT        NOT NULL,
    priority        TEXT        NOT NULL,
    -- Priority is ordered (min < low < default < high < urgent) and its textual
    -- form is not, so `?min_priority=` needs the rank rather than the name.
    priority_rank   SMALLINT    NOT NULL,
    title           TEXT        NOT NULL,
    body            TEXT,
    tags            TEXT[]      NOT NULL,
    group_key       TEXT,

    -- Written by the adapter on every head change rather than generated: a
    -- generated column has to be immutable, and `array_to_string` is only stable.
    -- Keeping it explicit also keeps the text configuration ('simple' — no
    -- stemming, because tags and identifiers are not prose) in one visible place.
    search          TSVECTOR    NOT NULL
);

-- One live notification per (channel, external id). Partial, because a job whose
-- previous notification aged out must be able to create a new one rather than
-- collide with a tombstone.
CREATE UNIQUE INDEX notifications_external_id_idx
    ON notifications (channel_id, external_id)
    WHERE external_id IS NOT NULL AND deleted_at IS NULL;

-- The inbox read: one channel, most recent activity first, tombstones excluded.
CREATE INDEX notifications_inbox_idx
    ON notifications (channel_id, cursor_seq DESC)
    WHERE deleted_at IS NULL;

-- The purge sweep, which asks only for what is due.
CREATE INDEX notifications_expiry_idx
    ON notifications (expires_at)
    WHERE deleted_at IS NULL AND expires_at IS NOT NULL;

-- The second purge pass: rows whose tombstone has had time to reach everyone.
CREATE INDEX notifications_deleted_idx
    ON notifications (deleted_at)
    WHERE deleted_at IS NOT NULL;

CREATE INDEX notifications_search_idx ON notifications USING GIN (search);
CREATE INDEX notifications_tags_idx ON notifications USING GIN (tags);

CREATE TABLE revisions (
    id               TEXT PRIMARY KEY,
    notification_id  TEXT        NOT NULL REFERENCES notifications (id) ON DELETE CASCADE,
    seq              INTEGER     NOT NULL,
    author           JSONB       NOT NULL,
    received_at      TIMESTAMPTZ NOT NULL,
    state            TEXT        NOT NULL,
    status           TEXT        NOT NULL,
    priority         TEXT        NOT NULL,
    content          JSONB       NOT NULL,
    -- Where this revision's event landed; the timeline pages on it.
    cursor_seq       BIGINT      NOT NULL,
    -- The domain assigns seq and applies last-write-wins; this index is what stops
    -- two concurrent publishers from both believing they won.
    UNIQUE (notification_id, seq)
);

CREATE INDEX revisions_timeline_idx ON revisions (notification_id, cursor_seq);

-- --------------------------------------------------------------------------
-- The event log — the single ordering authority (ADR-0012)
-- --------------------------------------------------------------------------

CREATE TABLE events (
    seq              BIGSERIAL PRIMARY KEY,
    -- 'channel' or 'user'. A preference event goes to one user's own connections
    -- and to nobody else's, however many channels they share (T26) — putting the
    -- audience in the row is what makes every transport get that right for free.
    audience_kind    TEXT        NOT NULL,
    channel_id       TEXT,
    user_id          TEXT,
    recorded_at      TIMESTAMPTZ NOT NULL,
    -- The `event:` name, plus one internal kind: 'expired_unseen'. That row
    -- consumes a sequence value but is never replayed — a notification that
    -- expired with nobody having seen it becomes a per-channel count instead of a
    -- backlog entry nobody can act on (design §7.2bis, T9).
    kind             TEXT        NOT NULL,
    notification_id  TEXT,
    payload          JSONB,
    CONSTRAINT events_audience_ck CHECK (
        (audience_kind = 'channel' AND channel_id IS NOT NULL AND user_id IS NULL)
        OR (audience_kind = 'user' AND user_id IS NOT NULL AND channel_id IS NULL)
    )
);

-- Catch-up: everything after a cursor, for a set of channels, in order.
CREATE INDEX events_channel_idx ON events (channel_id, seq) WHERE channel_id IS NOT NULL;
CREATE INDEX events_user_idx ON events (user_id, seq) WHERE user_id IS NOT NULL;
-- Compaction folds revisions of the same notification within a window.
CREATE INDEX events_notification_idx ON events (notification_id, seq)
    WHERE notification_id IS NOT NULL;
-- Retention, which is its own job on its own schedule (ADR-0012, accepted debt).
CREATE INDEX events_recorded_at_idx ON events (recorded_at);

-- --------------------------------------------------------------------------
-- Receipts and interactions
-- --------------------------------------------------------------------------

CREATE TABLE receipts (
    id               TEXT PRIMARY KEY,
    notification_id  TEXT        NOT NULL REFERENCES notifications (id) ON DELETE CASCADE,
    principal_ref    TEXT        NOT NULL,
    principal        JSONB       NOT NULL,
    -- Whether the principal is a person. "Seen by 3/5" counts humans, so a service
    -- account reading a channel must never move that number (§5.2bis, T25).
    is_human         BOOLEAN     NOT NULL,
    device_id        TEXT,
    kind             TEXT        NOT NULL,
    at               TIMESTAMPTZ NOT NULL
);

-- Idempotent per (principal, device, notification, kind): a client retrying a
-- delivery acknowledgement must not inflate the tally. NULL device is a distinct
-- value here, hence the coalesce rather than a plain unique constraint.
CREATE UNIQUE INDEX receipts_unique_idx
    ON receipts (notification_id, principal_ref, coalesce(device_id, ''), kind);

CREATE INDEX receipts_notification_idx ON receipts (notification_id);

CREATE TABLE interactions (
    id               TEXT PRIMARY KEY,
    notification_id  TEXT        NOT NULL REFERENCES notifications (id) ON DELETE CASCADE,
    author_ref       TEXT        NOT NULL,
    author           JSONB       NOT NULL,
    -- 'ack', 'reply' or 'choice' — the uniqueness rules differ per kind.
    interaction_kind TEXT        NOT NULL,
    -- The action key this concerns, for ack and choice.
    action_key       TEXT,
    interaction      JSONB       NOT NULL,
    recorded_at      TIMESTAMPTZ NOT NULL
);

-- An acknowledgement is idempotent per (notification, author, action). The domain
-- decides this from a snapshot; two concurrent acks are a race it cannot see, so
-- the index decides it again.
CREATE UNIQUE INDEX interactions_ack_idx
    ON interactions (notification_id, author_ref, action_key)
    WHERE interaction_kind = 'ack';

-- A pick is exclusive per (notification, author, action group): choosing again
-- replaces, so there is only ever one row to replace.
CREATE UNIQUE INDEX interactions_choice_idx
    ON interactions (notification_id, author_ref, action_key)
    WHERE interaction_kind = 'choice';

CREATE INDEX interactions_notification_idx ON interactions (notification_id, recorded_at);

CREATE TABLE silences (
    user_id     TEXT        NOT NULL,
    -- The target rendered as text, so re-silencing the same thing extends the
    -- existing silence rather than stacking a second one nobody can see to lift.
    target_ref  TEXT        NOT NULL,
    target      JSONB       NOT NULL,
    since       TIMESTAMPTZ NOT NULL,
    -- NULL means until it is lifted by hand.
    until       TIMESTAMPTZ,
    PRIMARY KEY (user_id, target_ref)
);

CREATE INDEX silences_user_idx ON silences (user_id);

-- --------------------------------------------------------------------------
-- Idempotency reservations (§8, T16)
-- --------------------------------------------------------------------------

CREATE TABLE idempotency_keys (
    principal_ref  TEXT        NOT NULL,
    key            TEXT        NOT NULL,
    -- NULL while the work is in flight; set once it succeeds, and replayed
    -- verbatim for the rest of the window.
    response       TEXT,
    claimed_at     TIMESTAMPTZ NOT NULL,
    expires_at     TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (principal_ref, key)
);

CREATE INDEX idempotency_expiry_idx ON idempotency_keys (expires_at);

-- --------------------------------------------------------------------------
-- Rate limits (§8.2, T14)
-- --------------------------------------------------------------------------

-- Shared on purpose: with several replicas an in-process counter enforces
-- `n × 60/min` rather than `60/min`. A fixed window rather than a sliding one —
-- the burst allowance in §8.2 already covers the edge, and a sliding window costs
-- a row per request.
CREATE TABLE rate_limits (
    quota_key     TEXT        PRIMARY KEY,
    window_start  TIMESTAMPTZ NOT NULL,
    count         INTEGER     NOT NULL
);

CREATE INDEX rate_limits_window_idx ON rate_limits (window_start);

-- --------------------------------------------------------------------------
-- Audit log (§13)
-- --------------------------------------------------------------------------

-- Append-only by convention and by the absence of any UPDATE or DELETE in the
-- adapter: this is the log replayed after an incident.
-- `seq` is this table's own pagination cursor. Deliberately not `events.seq`: an
-- administrative act is not something a client replays, and borrowing the stream's
-- sequence would put "somebody revoked a key" into everyone's backlog.
CREATE TABLE audit_entries (
    seq         BIGSERIAL PRIMARY KEY,
    at          TIMESTAMPTZ NOT NULL,
    actor_ref   TEXT        NOT NULL,
    actor       JSONB       NOT NULL,
    action      TEXT        NOT NULL,
    target      TEXT,
    channel_id  TEXT,
    detail      JSONB
);

CREATE INDEX audit_at_idx ON audit_entries (at DESC, seq DESC);
CREATE INDEX audit_actor_idx ON audit_entries (actor_ref, seq DESC);
CREATE INDEX audit_action_idx ON audit_entries (action, seq DESC);
CREATE INDEX audit_channel_idx ON audit_entries (channel_id, seq DESC) WHERE channel_id IS NOT NULL;
