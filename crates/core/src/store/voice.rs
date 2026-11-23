//! Voice messages on their way to text: the transcription worker's queue and the relink
//! step's. See migration 0009 for what each `transcript_status` means.

use chrono::{DateTime, Utc};
use sqlx::postgres::PgPool;
use sqlx::{query, query_as};

/// The item types that are voice messages: `voice_media` from the REST era, `AUDIOS`
/// from GraphQL.
macro_rules! is_voice {
    () => {
        "item_type IN ('voice_media', 'AUDIOS')"
    };
}

/// A voice message waiting for its words.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WaitingVoice {
    pub account_id: String,
    pub thread_id: String,
    pub item_id: String,
    /// The message as stored, which carries the link it arrived with.
    pub raw: serde_json::Value,
    /// A fresh link a re-read stored after the first one expired.
    pub voice_url: Option<String>,
}

/// Up to `limit` voice messages waiting for their words, newest first.
pub async fn waiting_voice(pool: &PgPool, limit: i64) -> sqlx::Result<Vec<WaitingVoice>> {
    query_as(concat!(
        "SELECT account_id, thread_id, item_id, raw, voice_url FROM message \
          WHERE ",
        is_voice!(),
        " AND transcript_status IS NULL AND deleted_at IS NULL \
          ORDER BY ts DESC LIMIT $1"
    ))
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// What became of a voice message: its words (`done`), or `failed` / `expired`.
pub async fn set_transcript(
    pool: &PgPool,
    voice: &WaitingVoice,
    transcript: Option<&str>,
    status: &str,
) -> sqlx::Result<()> {
    query(
        "UPDATE message SET transcript = $4, transcript_status = $5 \
          WHERE account_id = $1 AND thread_id = $2 AND item_id = $3",
    )
    .bind(&voice.account_id)
    .bind(&voice.thread_id)
    .bind(&voice.item_id)
    .bind(transcript)
    .bind(status)
    .execute(pool)
    .await?;
    Ok(())
}

/// A voice message whose link expired, and where a re-read finds it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExpiredVoice {
    pub thread_id: String,
    pub item_id: String,
    pub ts: DateTime<Utc>,
    /// The next newer message in the thread: a page asked for what is older than it
    /// starts with this one. `None` when it is the newest, which the newest page holds.
    pub next_item_id: Option<String>,
}

/// Up to `limit` voice messages of `account_id` whose link expired, newest first.
pub async fn expired_voice(
    pool: &PgPool,
    account_id: &str,
    limit: i64,
) -> sqlx::Result<Vec<ExpiredVoice>> {
    query_as(concat!(
        "SELECT m.thread_id, m.item_id, m.ts, \
                (SELECT n.item_id FROM message n \
                  WHERE n.account_id = m.account_id AND n.thread_id = m.thread_id \
                    AND (n.ts, n.item_id) > (m.ts, m.item_id) \
                  ORDER BY n.ts, n.item_id LIMIT 1) AS next_item_id \
           FROM message m \
          WHERE m.account_id = $1 AND m.transcript_status = 'expired' AND m.",
        is_voice!(),
        " ORDER BY m.ts DESC LIMIT $2"
    ))
    .bind(account_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// A re-read found a fresh link: the voice message waits for the worker again.
pub async fn relink_voice(
    pool: &PgPool,
    account_id: &str,
    voice: &ExpiredVoice,
    url: &str,
) -> sqlx::Result<()> {
    query(
        "UPDATE message SET voice_url = $4, transcript_status = NULL \
          WHERE account_id = $1 AND thread_id = $2 AND item_id = $3 \
            AND transcript_status = 'expired'",
    )
    .bind(account_id)
    .bind(&voice.thread_id)
    .bind(&voice.item_id)
    .bind(url)
    .execute(pool)
    .await?;
    Ok(())
}

/// A re-read did not find the voice message, or found it without a link: given up.
pub async fn give_up_voice(
    pool: &PgPool,
    account_id: &str,
    voice: &ExpiredVoice,
) -> sqlx::Result<()> {
    query(
        "UPDATE message SET transcript_status = 'failed' \
          WHERE account_id = $1 AND thread_id = $2 AND item_id = $3",
    )
    .bind(account_id)
    .bind(&voice.thread_id)
    .bind(&voice.item_id)
    .execute(pool)
    .await?;
    Ok(())
}
