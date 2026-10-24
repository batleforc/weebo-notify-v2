-- Each of these repeats the leading column of a primary key or unique index that
-- already serves the same lookups; keeping them only costs every write.
--   receipts_notification_idx      ← receipts_unique_idx (notification_id, …)
--   silences_user_idx              ← silences PRIMARY KEY (user_id, target_ref)
--   managed_declarations_kind_idx  ← managed_declarations PRIMARY KEY (kind, key)
DROP INDEX IF EXISTS receipts_notification_idx;
DROP INDEX IF EXISTS silences_user_idx;
DROP INDEX IF EXISTS managed_declarations_kind_idx;
