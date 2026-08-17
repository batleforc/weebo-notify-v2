-- What GitOps last applied, so the next pass can say what changed (design §11,
-- §13, D28).
--
-- §13 promises that trust policy changes are in the log an operator replays after
-- an incident. Nothing emitted them, and the reason was not laziness: the
-- reconciler applies the whole file every pass, so it knows what it is applying
-- and never what *moved*. For channels and service accounts it could have diffed
-- the live rows; for trust policies there was nothing at all, since those are read
-- into memory at boot and stored nowhere.
--
-- This is the missing previous state, and it deliberately stores the **declaration**
-- rather than the resulting entity. A managed channel's live row drifts for reasons
-- that are not config — a manager edits what they are allowed to edit — and diffing
-- against it would report those as GitOps changes. What §13 wants to know is
-- whether the *file* changed.
CREATE TABLE managed_declarations (
    -- `trust_policy`, `channel` or `service_account`. Not an enum type: this is a
    -- private bookkeeping table, and a new managed kind should not need a migration
    -- to be audited.
    kind        TEXT        NOT NULL,
    -- Identity within the kind: a channel id, an account id, or a trust policy's
    -- issuer. Stable across passes, which is what makes "added" and "removed"
    -- distinguishable from "changed".
    key         TEXT        NOT NULL,
    -- The declaration as applied, canonicalised by the caller. Compared whole:
    -- what changed inside it goes in the audit entry's `detail`, not here.
    snapshot    JSONB       NOT NULL,
    applied_at  TIMESTAMPTZ NOT NULL,

    PRIMARY KEY (kind, key)
);

-- Every read is "everything of this kind", because a pass diffs a whole set at
-- once: it has to see the rows that are about to disappear, which a lookup by key
-- can never return.
CREATE INDEX managed_declarations_kind_idx ON managed_declarations (kind);
