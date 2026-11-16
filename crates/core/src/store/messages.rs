//! Every read over `message`: one thread's history, full text, the share listing and
//! the revision log.
//!
//! The tools layer maps nothing itself — it reads these structs and hands them to the
//! renderer.

use chrono::{DateTime, Utc};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::types::Json;
use sqlx::{Row, query, query_scalar};

use crate::models::{Identity, Message, Reaction, Revision, ShareOwner};

/// One message out of any row selected with `message_columns!()`.
fn message_from_row(row: &PgRow) -> Result<Message, sqlx::Error> {
    Ok(Message {
        account_id: row.try_get("account_id")?,
        thread_id: row.try_get("thread_id")?,
        item_id: row.try_get("item_id")?,
        ts: row.try_get("ts")?,
        sender_id: row.try_get("sender_id")?,
        item_type: row.try_get("item_type")?,
        text: row.try_get("text")?,
        deleted_at: row.try_get("deleted_at")?,
        edited: row.try_get("edited")?,
        share_kind: row.try_get("share_kind")?,
        share_owner: row.try_get("share_owner")?,
        share_caption: row.try_get("share_caption")?,
        share_permalink: row.try_get("share_permalink")?,
        reply_to_item_id: row.try_get("reply_to_item_id")?,
        // Deliberately strict. Tolerating a bare string here is what hid slack-sync's
        // double-encoding bug: the decode undid the extra encode and only SQL noticed.
        reactions: row.try_get::<Json<Vec<Reaction>>, _>("reactions")?.0,
    })
}

/// `total_matches` is the size of the whole match set, not of this page: how many rows
/// the request would have returned without the `LIMIT`.
#[derive(Debug, Clone, Default)]
pub struct MessagePage {
    pub messages: Vec<Message>,
    pub total_matches: i64,
}

impl MessagePage {
    fn from_rows(rows: Vec<PgRow>) -> Result<Self, sqlx::Error> {
        let total_matches = match rows.first() {
            Some(row) => row.try_get("total_matches")?,
            None => 0,
        };
        let messages = rows
            .iter()
            .map(message_from_row)
            .collect::<Result<_, _>>()?;
        Ok(Self {
            messages,
            total_matches,
        })
    }
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub message: Message,
    /// `ts_headline`'s fragment. Worth a line only when it is not simply the message
    /// again, which it is for anything short enough to render whole.
    pub snippet: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SearchPage {
    pub hits: Vec<SearchHit>,
    pub total_matches: i64,
}

// One page of a thread, oldest first. Two statements, not one with a computed
// direction, because no caller value may ever reach the SQL text.
//
// With `after` the page is the *oldest* `limit` messages after it: a caller who names a
// start wants to read forward from there, and handing back the end of the period
// instead reads as though everything before it were missing. Without `after` it is the
// newest `limit`, so opening a thread shows what is going on now rather than its first
// fifty messages from years ago; `before` walks back from there.
//
// Ordering is `(ts, item_id)` in both directions, matching `message_order_idx`, so a
// page boundary can neither repeat nor skip a row when two items share a timestamp.
const HISTORY_FROM_OLDEST: &str = concat!(
    "SELECT * FROM (
      SELECT ",
    message_columns!(),
    ", ",
    total_matches!(),
    "
        FROM message
       WHERE thread_id = $1
         AND ($2::text IS NULL OR account_id = $2)
         AND ($3::timestamptz IS NULL OR ts > $3)
         AND ($4::timestamptz IS NULL OR ts < $4)
       ORDER BY ts, item_id
       LIMIT $5
    ) page
    ORDER BY ts, item_id"
);

const HISTORY_NEWEST: &str = concat!(
    "SELECT * FROM (
      SELECT ",
    message_columns!(),
    ", ",
    total_matches!(),
    "
        FROM message
       WHERE thread_id = $1
         AND ($2::text IS NULL OR account_id = $2)
         AND ($3::timestamptz IS NULL OR ts > $3)
         AND ($4::timestamptz IS NULL OR ts < $4)
       ORDER BY ts DESC, item_id DESC
       LIMIT $5
    ) page
    ORDER BY ts, item_id"
);

/// One thread's page of messages, oldest first. See [`HISTORY_FROM_OLDEST`] for which
/// page.
pub async fn get_history(
    pool: &PgPool,
    thread_id: &str,
    account: Option<&str>,
    after: Option<DateTime<Utc>>,
    before: Option<DateTime<Utc>>,
    limit: i64,
) -> Result<MessagePage, sqlx::Error> {
    let sql = if after.is_some() {
        HISTORY_FROM_OLDEST
    } else {
        HISTORY_NEWEST
    };
    let rows = query(sql)
        .bind(thread_id)
        .bind(account)
        .bind(after)
        .bind(before)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    MessagePage::from_rows(rows)
}

/// Full text over `message.search`, ranked, with a headline per hit.
///
/// An empty query returns nothing rather than everything: `websearch_to_tsquery` yields
/// an empty tsquery that matches no row, and that is the honest answer to a search for
/// nothing.
///
/// The tsvector is built from `text`, and `ingest.normalize` writes a share's author
/// handle and caption into `text` — so `@someone` and the words of a reel's caption are
/// searchable here without a single share-specific predicate.
#[allow(clippy::too_many_arguments)]
pub async fn search_messages(
    pool: &PgPool,
    search: &str,
    account: Option<&str>,
    thread_id: Option<&str>,
    senders: Option<&[Identity]>,
    kind: Option<&str>,
    after: Option<DateTime<Utc>>,
    before: Option<DateTime<Utc>>,
    limit: i64,
) -> Result<SearchPage, sqlx::Error> {
    if search.trim().is_empty() {
        return Ok(SearchPage::default());
    }
    let sender_accounts: Option<Vec<String>> =
        senders.map(|ids| ids.iter().map(|i| i.account_id.clone()).collect());
    let sender_ids: Option<Vec<String>> =
        senders.map(|ids| ids.iter().map(|i| i.ig_user_id.clone()).collect());

    let rows = query(concat!(
        "SELECT ",
        message_columns!(),
        ", ",
        total_matches!(),
        ",
               ts_rank_cd(search, websearch_to_tsquery('simple', $1)) AS rank,
               ts_headline('simple', text, websearch_to_tsquery('simple', $1),
                           'MaxFragments=1,MaxWords=25,MinWords=8') AS snippet
          FROM message
         WHERE search @@ websearch_to_tsquery('simple', $1)
           AND ($2::text IS NULL OR account_id = $2)
           AND ($3::text IS NULL OR thread_id = $3)
           AND ($4::text IS NULL OR share_kind = $4)
           AND ($5::timestamptz IS NULL OR ts > $5)
           AND ($6::timestamptz IS NULL OR ts < $6)
           AND ($7::text[] IS NULL OR (account_id, sender_id)
                IN (SELECT * FROM unnest($7::text[], $8::text[])))
         ORDER BY rank DESC, ts DESC, item_id DESC
         LIMIT $9"
    ))
    .bind(search)
    .bind(account)
    .bind(thread_id)
    .bind(kind)
    .bind(after)
    .bind(before)
    .bind(sender_accounts.as_deref())
    .bind(sender_ids.as_deref())
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let total_matches = match rows.first() {
        Some(row) => row.try_get("total_matches")?,
        None => 0,
    };
    let hits = rows
        .iter()
        .map(|row| {
            Ok(SearchHit {
                message: message_from_row(row)?,
                snippet: row.try_get("snippet")?,
            })
        })
        .collect::<Result<_, sqlx::Error>>()?;
    Ok(SearchPage {
        hits,
        total_matches,
    })
}

/// Every reel, post, story and link ever sent or received, newest first.
///
/// `owner` is the handle of whoever AUTHORED the thing, not whoever forwarded it, and
/// it is matched through `lower()` to hit `message_share_owner_idx` — Instagram handles
/// are case-insensitive and this one is typed by hand at the query end.
#[allow(clippy::too_many_arguments)]
pub async fn list_shares(
    pool: &PgPool,
    account: Option<&str>,
    owner: Option<&str>,
    kind: Option<&str>,
    thread_id: Option<&str>,
    after: Option<DateTime<Utc>>,
    before: Option<DateTime<Utc>>,
    limit: i64,
) -> Result<MessagePage, sqlx::Error> {
    let rows = query(concat!(
        "SELECT ",
        message_columns!(),
        ", ",
        total_matches!(),
        "
          FROM message
         WHERE share_kind IS NOT NULL
           AND ($1::text IS NULL OR account_id = $1)
           AND ($2::text IS NULL OR lower(share_owner) = lower($2))
           AND ($3::text IS NULL OR share_kind = $3)
           AND ($4::text IS NULL OR thread_id = $4)
           AND ($5::timestamptz IS NULL OR ts > $5)
           AND ($6::timestamptz IS NULL OR ts < $6)
         ORDER BY ts DESC, item_id DESC
         LIMIT $7"
    ))
    .bind(account)
    .bind(owner)
    .bind(kind)
    .bind(thread_id)
    .bind(after)
    .bind(before)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    MessagePage::from_rows(rows)
}

/// The most-shared authors, so a caller can find a handle worth asking `owner` for.
///
/// Without this the shares tool answers only questions whose answer you already knew.
pub async fn share_owners(
    pool: &PgPool,
    account: Option<&str>,
    limit: i64,
) -> Result<Vec<ShareOwner>, sqlx::Error> {
    let rows = query(
        "
        SELECT lower(share_owner) AS owner, count(*) AS shares, max(ts) AS last_shared_at
          FROM message
         WHERE share_owner IS NOT NULL
           AND ($1::text IS NULL OR account_id = $1)
         GROUP BY lower(share_owner)
         ORDER BY shares DESC, owner
         LIMIT $2
        ",
    )
    .bind(account)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| {
            Ok(ShareOwner {
                owner: row.try_get("owner")?,
                shares: row.try_get("shares")?,
                last_shared_at: row.try_get("last_shared_at")?,
            })
        })
        .collect()
}

/// Whether the archive holds a message at all, and when it was deleted if it was.
///
/// Distinguishing "no such message" from "not deleted" matters: the tool answers them
/// differently and a NULL alone cannot carry both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    NoSuchMessage,
    Message { deleted_at: Option<DateTime<Utc>> },
}

pub async fn resolve_message(
    pool: &PgPool,
    account_id: &str,
    thread_id: &str,
    item_id: &str,
) -> Result<Resolved, sqlx::Error> {
    let found: Option<Option<DateTime<Utc>>> = query_scalar(
        "
        SELECT deleted_at FROM message
         WHERE account_id = $1 AND thread_id = $2 AND item_id = $3
        ",
    )
    .bind(account_id)
    .bind(thread_id)
    .bind(item_id)
    .fetch_optional(pool)
    .await?;
    Ok(match found {
        None => Resolved::NoSuchMessage,
        Some(deleted_at) => Resolved::Message { deleted_at },
    })
}

/// One message's text over time, oldest first.
pub async fn list_revisions(
    pool: &PgPool,
    account_id: &str,
    thread_id: &str,
    item_id: &str,
) -> Result<Vec<Revision>, sqlx::Error> {
    let rows = query(
        "
        SELECT seq, text, first_seen_at, superseded_at FROM message_revision
         WHERE account_id = $1 AND thread_id = $2 AND item_id = $3 ORDER BY seq
        ",
    )
    .bind(account_id)
    .bind(thread_id)
    .bind(item_id)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| {
            Ok(Revision {
                seq: row.try_get("seq")?,
                text: row.try_get("text")?,
                first_seen_at: row.try_get("first_seen_at")?,
                superseded_at: row.try_get("superseded_at")?,
            })
        })
        .collect()
}
