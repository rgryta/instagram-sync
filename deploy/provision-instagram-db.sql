-- Provision the `instagram` database and its two roles on homelab-postgres.
--
-- Run as the postgres superuser:
--   psql -h homelab-postgres -U postgres \
--     -v ingest_pw="<ingest password>" -v mcp_pw="<mcp password>" \
--     -f deploy/provision-instagram-db.sql
--
-- NOTE: pass RAW passwords. :'ingest_pw' quotes the value once; pre-quoting stores
-- the literal quotes in the role password and authentication then fails with a
-- message that says nothing about quoting.

CREATE ROLE instagram_ingest LOGIN PASSWORD :'ingest_pw';
CREATE ROLE instagram_mcp    LOGIN PASSWORD :'mcp_pw';

CREATE DATABASE instagram OWNER instagram_ingest;

\connect instagram

GRANT ALL ON SCHEMA public TO instagram_ingest;
GRANT USAGE ON SCHEMA public TO instagram_mcp;

-- Redundant on PostgreSQL 15 and above, which stopped granting CREATE on `public` to
-- PUBLIC, and harmless there. Kept because the read-only rule in this repo is a risk
-- control rather than a preference, and it should not depend on the server version
-- this database happens to be restored onto.
REVOKE CREATE ON SCHEMA public FROM PUBLIC;

-- Without CREATE on the tablespaces the migration that places them fails soft and the
-- corpus silently stays on the default volume. Guarded because a host without these
-- tablespaces is a valid host.
DO $$ BEGIN
  EXECUTE 'GRANT CREATE ON TABLESPACE archive_data, nvme_indexes TO instagram_ingest';
EXCEPTION WHEN undefined_object THEN
  RAISE NOTICE 'archive_data/nvme_indexes absent; the corpus will live on the default tablespace';
END $$;

-- The MCP role reads everything and writes nothing. Its only write authority is the
-- two alias tables, granted by 0001_init.sql at the ingester's first boot, because the
-- tables do not exist yet at this point.
--
-- No sequence grant: the schema has no serial columns, so there is no sequence to use.
ALTER DEFAULT PRIVILEGES FOR ROLE instagram_ingest IN SCHEMA public
  GRANT SELECT ON TABLES TO instagram_mcp;
