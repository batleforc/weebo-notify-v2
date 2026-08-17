-- One-shot memory for federated ID tokens (design §5.3, scenario T11).
--
-- A CI job's ID token exchanges exactly once. The `jti` is remembered until the
-- token's own `exp` and no longer: past that a replay fails on expiry anyway, so
-- keeping it would be storage bought for nothing.
--
-- The primary key is the whole mechanism. "Check then insert" is two statements
-- and therefore a race — two jobs presenting the same leaked token at the same
-- moment would both read "unseen" and both be minted a token. One INSERT whose
-- conflict means "already used" cannot be raced.
CREATE TABLE federation_replay (
    jti        TEXT PRIMARY KEY,
    -- The incoming token's expiry, which is what the purge sweeps on.
    expires_at TIMESTAMPTZ NOT NULL,
    seen_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The purge reads by expiry and never by id, so this is the index it needs.
CREATE INDEX federation_replay_expires_at_idx ON federation_replay (expires_at);
