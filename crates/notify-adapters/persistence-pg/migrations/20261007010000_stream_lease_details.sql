-- What a user sees of their live connections, and the credentials they disconnected.
ALTER TABLE stream_leases
    ADD COLUMN transport   text        NOT NULL DEFAULT 'sse',
    ADD COLUMN client      text        NOT NULL DEFAULT '',
    ADD COLUMN credential  text        NOT NULL DEFAULT 'session',
    ADD COLUMN fingerprint text        NOT NULL DEFAULT '',
    ADD COLUMN opened_at   timestamptz NOT NULL DEFAULT now();

CREATE UNLOGGED TABLE stream_blocks (
    fingerprint text        PRIMARY KEY,
    until       timestamptz NOT NULL
);
