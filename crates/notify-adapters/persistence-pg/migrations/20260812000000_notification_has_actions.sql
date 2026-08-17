-- Whether the head revision carries at least one action button (design §2.3,
-- §10.4). Denormalised for the same reason state/status/priority/tags/group_key
-- already are (see the module doc in `notifications.rs`): the inbox filters on it,
-- and `content` is JSONB the index cannot see into.
ALTER TABLE notifications ADD COLUMN has_actions BOOLEAN NOT NULL DEFAULT false;
