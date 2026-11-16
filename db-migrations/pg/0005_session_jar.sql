-- The ingester's Instagram session: the cookie jar, as Playwright's `context.cookies()`
-- JSON. Instagram rotates `sessionid` and friends through `Set-Cookie`, and a jar that
-- forgets a rotation is logged out, so it lives here rather than in the environment.
--
-- No foreign key to `account`: the jar is seeded before the first pass has read who the
-- session belongs to, which is what creates the account row.

CREATE TABLE ingest_session (
  account_id text        PRIMARY KEY,
  cookies    jsonb       NOT NULL,
  updated_at timestamptz NOT NULL DEFAULT now()
);

-- `deploy/provision-instagram-db.sql` grants the MCP role SELECT on every table the
-- ingester creates. This one is a live login, and the MCP process must never hold one.
-- Guarded as 0001_init.sql's grant is: the role does not exist on a test container.
DO $$ BEGIN
  EXECUTE 'REVOKE ALL ON ingest_session FROM instagram_mcp';
EXCEPTION WHEN undefined_object THEN NULL;
END $$;
