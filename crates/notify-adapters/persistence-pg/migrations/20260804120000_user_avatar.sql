-- Avatars (design §2.4, M2.2).
--
-- The reference only: the bytes live in the bucket, because a 256×256 PNG in a
-- row is a payload every query that touches a user carries whether it wanted it or
-- not. An id here and an object there also means deleting the object is a decision
-- the purge job can make on its own (§7.2bis).
ALTER TABLE users ADD COLUMN avatar TEXT;
