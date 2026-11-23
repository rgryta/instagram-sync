-- Voice messages as text. The ingester's worker fetches each one's audio from Meta's CDN
-- by the link the message carries and has the whisper.cpp server transcribe it; only the
-- words are stored, in a column of their own (not typed), which search covers too.
--
-- That link expires after a few days. A voice message still waits while
-- `transcript_status` is NULL; `done` holds its words; `expired` is a link the CDN
-- refused, until a re-read of the message's page stores a fresh one in `voice_url` and
-- clears the status; `failed` is final (whisper refused it, or a fresh link was refused).
ALTER TABLE message ADD COLUMN transcript text;
ALTER TABLE message ADD COLUMN transcript_status text
  CHECK (transcript_status IN ('done', 'failed', 'expired'));
ALTER TABLE message ADD COLUMN voice_url text;

-- One rewrite of the table. The fuzzy (trigram) match stays on `text`: a transcript is
-- found by its words, accents folded, not by a misspelling of them.
ALTER TABLE message ALTER COLUMN search
  SET EXPRESSION AS (to_tsvector('simple_fold', coalesce(text, '') || ' ' || coalesce(transcript, '')));
