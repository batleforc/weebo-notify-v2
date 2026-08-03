-- User profiles (design §2.4, M2.1).
--
-- `display_name` already exists: it is what every embedded `author` reads, so it
-- stays a column of its own rather than moving into the profile. What arrives here
-- is the rest of what somebody chooses, plus the one column the feature turns on.
--
-- **`overridden_fields` is the feature, not bookkeeping.** §2.4 asks that a field
-- edited locally stop being overwritten by later OIDC syncs; without a record of
-- which fields were claimed there are only two possible behaviours and both are
-- wrong — the provider overwrites a choice at every login, or it never refreshes
-- anything again. A `text[]` rather than jsonb because it is read back whole and
-- because the domain already parses `text[]` filters elsewhere.
ALTER TABLE users
    ADD COLUMN timezone          TEXT,
    ADD COLUMN locale            TEXT,
    ADD COLUMN overridden_fields TEXT[] NOT NULL DEFAULT '{}';
