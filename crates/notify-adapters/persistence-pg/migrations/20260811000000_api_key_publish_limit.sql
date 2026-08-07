-- A platform-admin's ceiling on one key's own publish rate (design §8.2, D24).
--
-- `NULL` means "no override" — the channel's own `publish_per_minute` still
-- applies, this only narrows it further for one noisy key. `BIGINT` rather than
-- `INTEGER`: the domain value is `u32`, and widening the column is cheaper than
-- teaching every write site to check it fits in `i32` first.
ALTER TABLE api_keys ADD COLUMN publish_per_minute BIGINT;
