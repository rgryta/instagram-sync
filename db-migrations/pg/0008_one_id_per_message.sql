-- One id per message: the GraphQL `message_id` (`mid.$…`).
--
-- Rows the REST era stored are keyed on its numeric `item_id`. GraphQL pages name the
-- same message by `message_id`, which the REST payload also carried, so a backfill that
-- re-walked REST-era history stored every message it met a second time. Re-keying the
-- REST rows to their `message_id` makes the primary key refuse that second copy. A
-- numeric id is also useless as a backfill resume point: `older_than_message_id`
-- answers it with an empty last page, which reads as the start of the thread.
--
-- Where both copies exist the GraphQL row stays; its edit history, if it has none,
-- takes the REST row's. Replies and poll watermarks that name a REST id follow it.

ALTER TABLE thread ADD COLUMN backfill_at timestamptz;

CREATE TEMP TABLE rest_id ON COMMIT DROP AS
SELECT m.account_id, m.thread_id, m.item_id AS old_id, m.raw ->> 'message_id' AS mid,
       EXISTS (SELECT 1 FROM message g
                WHERE g.account_id = m.account_id AND g.thread_id = m.thread_id
                  AND g.item_id = m.raw ->> 'message_id') AS twin
  FROM message m
 WHERE m.item_id ~ '^[0-9]+$' AND m.raw ->> 'message_id' LIKE 'mid.%';

UPDATE message m SET reply_to_item_id = r.mid
  FROM rest_id r
 WHERE m.account_id = r.account_id AND m.thread_id = r.thread_id
   AND m.reply_to_item_id = r.old_id;

UPDATE thread t SET watermark_item_id = r.mid
  FROM rest_id r
 WHERE t.account_id = r.account_id AND t.thread_id = r.thread_id
   AND t.watermark_item_id = r.old_id;

UPDATE message_revision v SET item_id = r.mid
  FROM rest_id r
 WHERE r.twin AND v.account_id = r.account_id AND v.thread_id = r.thread_id
   AND v.item_id = r.old_id
   AND NOT EXISTS (SELECT 1 FROM message_revision w
                    WHERE w.account_id = r.account_id AND w.thread_id = r.thread_id
                      AND w.item_id = r.mid);

DELETE FROM message m
 USING rest_id r
 WHERE r.twin AND m.account_id = r.account_id AND m.thread_id = r.thread_id
   AND m.item_id = r.old_id;

ALTER TABLE message_revision
  DROP CONSTRAINT message_revision_account_id_thread_id_item_id_fkey;
ALTER TABLE message_revision
  ADD CONSTRAINT message_revision_account_id_thread_id_item_id_fkey
  FOREIGN KEY (account_id, thread_id, item_id)
  REFERENCES message (account_id, thread_id, item_id) ON DELETE CASCADE ON UPDATE CASCADE;

UPDATE message m SET item_id = r.mid
  FROM rest_id r
 WHERE NOT r.twin AND m.account_id = r.account_id AND m.thread_id = r.thread_id
   AND m.item_id = r.old_id;
