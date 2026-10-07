-- One row per live SSE/WebSocket connection, so the §8.2 ceiling holds across replicas.
-- Unlogged: losing it in a crash only forgets connections that died with the crash.
CREATE UNLOGGED TABLE stream_leases (
    id         uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    principal  text        NOT NULL,
    replica    text        NOT NULL,
    expires_at timestamptz NOT NULL
);

CREATE INDEX stream_leases_principal ON stream_leases (principal);
CREATE INDEX stream_leases_replica ON stream_leases (replica);
