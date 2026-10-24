-- A row backing off after a failure is not one a worker holds. Kept apart from
-- `leased_until`, so "another replica is pushing this notification" (the claim's
-- per-notification rule) does not block a newer revision behind a failing one.
ALTER TABLE fanout_outbox ADD COLUMN retry_at TIMESTAMPTZ;
