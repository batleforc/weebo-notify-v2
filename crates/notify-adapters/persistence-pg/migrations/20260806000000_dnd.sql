-- Do not disturb, per user (design §7.4, M3.1).
--
-- One row per user who has set one, and no row at all for everybody else: the port
-- answers `Dnd::off` for a missing row, so "never opened Settings" and "a
-- preference that mutes nothing" are the same thing to every caller — and seeding a
-- row per user at first login would put a write on the hottest path in the API for
-- a value that is already the default.
--
-- **jsonb rather than columns.** The preference is read and written whole (§7.4
-- carries it whole on the wire for the same reason) and never queried by its parts:
-- nothing asks "who is quiet on Tuesdays". Quiet hours are a list of windows, each
-- with its own days, ends and timezone, so columns would mean a second table joined
-- on every read for a value that has exactly one reader. The domain validates the
-- document on the way back out — `Dnd::validated`, same rules as ingestion — so a
-- row written by last month's build still has to satisfy this month's invariants.
CREATE TABLE user_dnd (
    user_id    TEXT        PRIMARY KEY,
    preference JSONB       NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);
