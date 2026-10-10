-- The index behind `GET /avatars/{id}` checking that an id is somebody's avatar.
--
-- Avatars and attachments share one object store, so the route has to ask "is this
-- a current avatar" before serving bytes — otherwise it reads any attachment by id
-- with no channel check. That question is asked on every rendered avatar, hence an
-- index; partial, because most users never set one.
CREATE INDEX IF NOT EXISTS users_avatar_idx ON users (avatar) WHERE avatar IS NOT NULL;
