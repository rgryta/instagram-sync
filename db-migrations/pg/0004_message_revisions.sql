-- Keep what a message used to say, and mark the ones that go away.
--
-- `_has_moved` already re-opens a thread whose listing moved without a new message,
-- and its own comment names the reason: "a thread that was edited or had a reaction
-- added moves it without adding a message we could key on". So the re-read was always
-- happening. `insert_messages` is insert-only and discarded the answer.
--
-- No backfill. Prior texts were never stored and cannot be recovered.

CREATE TABLE message_revision (
  account_id    text        NOT NULL,
  thread_id     text        NOT NULL,
  item_id       text        NOT NULL,
  seq           int         NOT NULL,
  text          text,
  -- NULL on seq 1 and only there, meaning unknown: nothing records when a message was
  -- first stored, so the earliest text held has no honest start time.
  first_seen_at timestamptz,
  superseded_at timestamptz,
  PRIMARY KEY (account_id, thread_id, item_id, seq),
  FOREIGN KEY (account_id, thread_id, item_id)
    REFERENCES message (account_id, thread_id, item_id) ON DELETE CASCADE
);

CREATE INDEX message_revision_seq_idx ON message_revision (account_id, thread_id, item_id, seq);

-- At most one current revision per message: a revision log that can disagree with the
-- message it describes leaves no way to tell which of the two is lying.
CREATE UNIQUE INDEX message_revision_one_current_idx
  ON message_revision (account_id, thread_id, item_id)
  WHERE superseded_at IS NULL;

-- One clean pass that fails to return a message we hold stamps missing_since; the
-- next one promotes it. An absence is also what a truncated page looks like, and
-- deleted_at hides a message from every default view.
ALTER TABLE message ADD COLUMN deleted_at    timestamptz;
ALTER TABLE message ADD COLUMN missing_since timestamptz;

DO $$
DECLARE
  has_archive boolean := EXISTS (SELECT 1 FROM pg_tablespace WHERE spcname = 'archive_data');
  has_nvme    boolean := EXISTS (SELECT 1 FROM pg_tablespace WHERE spcname = 'nvme_indexes');
BEGIN
  IF NOT (has_archive AND has_nvme) THEN
    RAISE NOTICE 'archive_data/nvme_indexes absent; leaving the new objects on the default tablespace';
    RETURN;
  END IF;
  SET LOCAL lock_timeout = '5s';
  EXECUTE 'ALTER TABLE message_revision SET TABLESPACE archive_data';
  EXECUTE 'ALTER INDEX message_revision_seq_idx SET TABLESPACE nvme_indexes';
  EXECUTE 'ALTER INDEX message_revision_one_current_idx SET TABLESPACE nvme_indexes';
END $$;
