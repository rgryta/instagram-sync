-- Lift the four facts about a shared item out of `raw` and onto the row.
--
-- Roughly half this corpus is not text. Measured over 168 captured items: 52% `text`,
-- 32% `clip` (a reel), then `media_share`, `reel_share`, `link`. The question this
-- archive exists to answer — "what did anyone ever send me from @someone, or about X" —
-- is therefore a question about shares, and answering it out of `raw` means a jsonb path
-- per item type in every query that asks. These columns make it one predicate.
--
-- 0001_init.sql deliberately stopped short of this: the payload shapes were unverified,
-- and a projection written against a guessed path is worse than none because it looks
-- authoritative. They have since been verified against a live account, item type by item
-- type, which is what makes this migration writable.
--
-- The values are derived in `instagram_sync/ingest/normalize.py`, not here. There is
-- exactly one implementation of "which key holds the author's handle for this type", and
-- it is the one that has the captures in its comments. A SQL expression that re-derived
-- it would be a second implementation, and the two would diverge on the first item type
-- Instagram adds.
--
-- No backfill. No database has been provisioned for this project and no rows exist
-- anywhere, so a backfill statement here would be dead code that ships forever and can
-- never be observed to work. That is a decision, not an oversight: `raw` is stored
-- verbatim precisely so that a backfill can be written later, against real rows, by
-- whoever needs one.

ALTER TABLE message
  -- Normalised across item types: 'reel' | 'post' | 'story' | 'link'.
  -- Not `item_type` under another name — it collapses Instagram's key-naming (`clip`,
  -- `direct_media_share`) into what the thing actually is, and is NULL for every item
  -- that shares nothing.
  ADD COLUMN IF NOT EXISTS share_kind      text,
  -- The handle of whoever AUTHORED the shared thing. `sender_id` is the person who
  -- forwarded it into the DM, and they are almost never the same person.
  ADD COLUMN IF NOT EXISTS share_owner     text,
  ADD COLUMN IF NOT EXISTS share_caption   text,
  -- NULL when the payload carries no shortcode. A story that has expired is gone from
  -- Instagram's side and no URL can be built for it; a fabricated one would be a link
  -- that 404s, which is worse than an honest absence.
  ADD COLUMN IF NOT EXISTS share_permalink text;

-- Both partial: about half of every row has no share at all, and those rows have no
-- business in an index that only ever serves "everything from @someone" and
-- "everything of this kind".
--
-- `lower(share_owner)` because Instagram handles are case-insensitive and are typed by
-- hand at the query end; the expression has to match the one the query uses.
CREATE INDEX IF NOT EXISTS message_share_owner_idx
  ON message (account_id, lower(share_owner)) WHERE share_owner IS NOT NULL;
CREATE INDEX IF NOT EXISTS message_share_kind_idx
  ON message (account_id, share_kind) WHERE share_kind IS NOT NULL;
