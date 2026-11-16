-- instagram-sync initial schema.
--
-- Deliberately the core only. Instagram DM payloads have not yet been observed against
-- a live account, and the sibling repos both paid for a parser written against guessed
-- shapes. `raw` holds every payload verbatim, so the projected columns that a later
-- migration adds (share owner, caption, permalink) can be backfilled out of rows that
-- are already stored, without refetching anything.
--
-- `item_id` is text and kept verbatim: it is Instagram's own identity for a message and
-- doubles as a tiebreak sort key. `ts` carries the parsed instant, which is what every
-- range query actually wants — Instagram sends the timestamp in MICROseconds, and the
-- conversion happens once, on the way in.

CREATE TABLE account (
  id                 text PRIMARY KEY,
  ig_user_id         text NOT NULL,
  username           text,
  enabled            boolean NOT NULL DEFAULT true,
  last_auth_error    text,
  last_auth_error_at timestamptz,
  updated_at         timestamptz NOT NULL DEFAULT now()
);

-- Everyone who appears in a thread, including the account itself. Instagram gives three
-- names per person and they are not interchangeable: `username` is the handle, and
-- `full_name` is what the client renders next to a message. Storing both is not
-- redundancy — filing someone under the name nobody in the app can see is a defect the
-- sibling repo shipped and had to migrate out of.
CREATE TABLE participant (
  account_id      text NOT NULL REFERENCES account(id),
  ig_user_id      text NOT NULL,
  username        text,
  full_name       text,
  is_verified     boolean NOT NULL DEFAULT false,
  profile_pic_url text,
  updated_at      timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (account_id, ig_user_id)
);
CREATE INDEX participant_username_idx ON participant (account_id, lower(username));

-- `participant_ids` denormalised onto the thread because a DM has no title: rendering
-- one at all requires knowing who is in it, and that lookup happens on every single row
-- of every listing.
CREATE TABLE thread (
  account_id       text NOT NULL REFERENCES account(id),
  thread_id        text NOT NULL,
  title            text,
  is_group         boolean NOT NULL DEFAULT false,
  participant_ids  text[] NOT NULL DEFAULT '{}',
  last_activity_at timestamptz,
  -- Newest item already stored. The poll loop compares Instagram's `last_activity_at`
  -- against this to decide whether a thread is worth opening at all, which is what
  -- keeps a pass at a handful of requests instead of one per thread.
  watermark_item_id text,
  backfill_cursor  text,
  backfill_done    boolean NOT NULL DEFAULT false,
  last_polled_at   timestamptz,
  PRIMARY KEY (account_id, thread_id)
);
CREATE INDEX thread_activity_idx ON thread (account_id, last_activity_at DESC);

-- to_tsvector('simple', ...) — no stemming. The corpus is mixed-language; 'english'
-- would silently drop hits on non-English text.
--
-- `reactions` is jsonb on the row rather than its own table, unlike slack-sync. An
-- Instagram reaction set is a handful of emoji on one item and is never queried
-- independently of the message it is on; a table would buy a join and nothing else.
CREATE TABLE message (
  account_id       text NOT NULL,
  thread_id        text NOT NULL,
  item_id          text NOT NULL,
  ts               timestamptz NOT NULL,
  sender_id        text,
  item_type        text NOT NULL,
  text             text NOT NULL DEFAULT '',
  reply_to_item_id text,
  reactions        jsonb NOT NULL DEFAULT '[]'::jsonb,
  raw              jsonb NOT NULL,
  search           tsvector GENERATED ALWAYS AS (to_tsvector('simple', coalesce(text,''))) STORED,
  PRIMARY KEY (account_id, thread_id, item_id),
  FOREIGN KEY (account_id, thread_id) REFERENCES thread(account_id, thread_id)
);
CREATE INDEX message_search_idx ON message USING gin (search);
-- (ts, item_id): Instagram ids sort by time in practice but that is an observation, not
-- a contract, and two items can share a timestamp. The id is the tiebreak so paging
-- never repeats or skips a row.
CREATE INDEX message_order_idx  ON message (account_id, thread_id, ts DESC, item_id DESC);
CREATE INDEX message_ts_idx     ON message (ts DESC);
CREATE INDEX message_sender_idx ON message (account_id, sender_id);
-- Shares are the reason this archive exists: the question "what did anyone ever send me
-- about X" is one no Instagram client can answer. Most items are not plain text.
CREATE INDEX message_type_idx   ON message (account_id, item_type);

CREATE TABLE person_alias (
  alias      text PRIMARY KEY,
  note       text,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now()
);

-- No participant FK: an alias may name someone before their thread has been synced, and
-- must survive them disappearing from `participant`.
CREATE TABLE person_alias_member (
  account_id text NOT NULL,
  ig_user_id text NOT NULL,
  alias      text NOT NULL REFERENCES person_alias(alias) ON DELETE CASCADE,
  PRIMARY KEY (account_id, ig_user_id)
);
CREATE INDEX person_alias_member_alias_idx ON person_alias_member (alias);

CREATE TABLE sync_state (
  account_id       text NOT NULL REFERENCES account(id),
  loop             text NOT NULL CHECK (loop IN ('poll','backfill')),
  current_thread_id text,
  last_run_at      timestamptz,
  last_error       text,
  last_error_at    timestamptz,
  PRIMARY KEY (account_id, loop)
);

-- The MCP role's only write authority. GRANT is not directly executable inside a DO
-- block (it must go through EXECUTE), and the role does not exist on a throwaway test
-- container, so the missing-role case is swallowed.
DO $$ BEGIN
  EXECUTE 'GRANT INSERT, UPDATE, DELETE ON person_alias, person_alias_member TO instagram_mcp';
EXCEPTION WHEN undefined_object THEN NULL;
END $$;
