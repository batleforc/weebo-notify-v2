-- `GET /serviceaccounts` pages by cursor like every other list (§8). Ids are slugs,
-- so a sequence gives the stable order a cursor needs; existing rows are numbered
-- as the column is added.
ALTER TABLE service_accounts ADD COLUMN seq BIGINT GENERATED ALWAYS AS IDENTITY;
CREATE UNIQUE INDEX service_accounts_seq_idx ON service_accounts (seq);
