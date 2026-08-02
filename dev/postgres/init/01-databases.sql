-- Runs once, on the first boot of an empty Postgres volume (design §12.1).
--
-- Two databases in one instance: the server's, created by POSTGRES_DB, and
-- Authentik's, created here so the `authentik` compose profile has somewhere to
-- live without a second Postgres container.

CREATE ROLE authentik WITH LOGIN PASSWORD 'authentik';
CREATE DATABASE authentik OWNER authentik;

-- The server owns its own schema; sqlx migrations run against weebo_notify.
GRANT ALL PRIVILEGES ON DATABASE weebo_notify TO weebo;
