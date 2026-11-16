//! The ingester's writes, and the few reads only the ingest loops make.
//!
//! Write ordering is forced by the foreign keys: account -> thread -> message, with
//! participant and sync_state hanging off the account. Nothing here creates a parent
//! implicitly; a message written without its thread fails, which is the point — a
//! stranded row that no listing can reach is worse than a loud failure.
//!
//! The split between [`upsert_thread`] and the `set_*` functions is the load-bearing
//! one. A thread row is re-upserted from every inbox listing, and a listing knows
//! nothing about crawl progress; the loops own `backfill_cursor`, `backfill_done` and
//! `watermark_item_id` and are the only writers of them.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::query_builder::Separated;
use sqlx::types::Json;
use sqlx::{Postgres, QueryBuilder, Row, query, query_as, query_scalar};

use crate::models::{AccountRecord, MessageRecord, ParticipantRecord, StoredThread, ThreadRecord};

/// Postgres accepts 65535 bind parameters per statement and each message spends one per
/// column. A page is nowhere near that, but a caller handing over a whole thread at once
/// should get more round trips rather than a bind-parameter error.
const INSERT_CHUNK: usize = 1000;

/// One list, used by the insert, by [`newest_message`] and by [`message_values`]. A
/// column added here and not there is a bind-count error from Postgres, not a silent
/// one-position shift of every value after it. Macros rather than consts so the
/// queries stay `&'static str`.
macro_rules! stored_message_columns {
    () => {
        "account_id, thread_id, item_id, ts, sender_id, item_type, text, \
         share_kind, share_owner, share_caption, share_permalink, reply_to_item_id, \
         reactions, raw"
    };
}

macro_rules! thread_columns {
    () => {
        "account_id, thread_id, title, is_group, participant_ids, last_activity_at, \
         watermark_item_id, backfill_cursor, backfill_done, last_polled_at"
    };
}

/// Insert or refresh the ingesting account.
///
/// `last_auth_error` is deliberately not in the UPDATE clause: it is written by
/// [`record_auth_error`] at the moment a session dies, and a routine account refresh
/// must not erase it.
pub async fn upsert_account(pool: &PgPool, account: &AccountRecord) -> Result<(), sqlx::Error> {
    query(
        "
        INSERT INTO account (id, ig_user_id, username, enabled, updated_at)
        VALUES ($1, $2, $3, $4, now())
        ON CONFLICT (id) DO UPDATE SET
          ig_user_id = EXCLUDED.ig_user_id,
          username   = COALESCE(EXCLUDED.username, account.username),
          enabled    = EXCLUDED.enabled,
          updated_at = now()
        ",
    )
    .bind(&account.id)
    .bind(&account.ig_user_id)
    .bind(&account.username)
    // `AccountRecord` has no `enabled`: the Python ingester always wrote its default,
    // True, so every pass re-enables the account exactly as it did there.
    .bind(true)
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert or refresh a batch of people.
///
/// Names arrive from several endpoints and not all of them carry both: an inbox listing
/// gives the handle, a thread payload gives the rendered name. Each is COALESCEd so the
/// thinner payload never blanks what the richer one already stored.
///
/// One transaction, because asyncpg's `executemany` was all-or-nothing.
pub async fn upsert_participants(
    pool: &PgPool,
    participants: &[ParticipantRecord],
) -> Result<(), sqlx::Error> {
    if participants.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    for p in participants {
        query(
            "
            INSERT INTO participant (
              account_id, ig_user_id, username, full_name, is_verified, profile_pic_url,
              updated_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, now())
            ON CONFLICT (account_id, ig_user_id) DO UPDATE SET
              username        = COALESCE(EXCLUDED.username, participant.username),
              full_name       = COALESCE(EXCLUDED.full_name, participant.full_name),
              is_verified     = EXCLUDED.is_verified,
              profile_pic_url = COALESCE(EXCLUDED.profile_pic_url, participant.profile_pic_url),
              updated_at      = now()
            ",
        )
        .bind(&p.account_id)
        .bind(&p.ig_user_id)
        .bind(&p.username)
        .bind(&p.full_name)
        .bind(p.is_verified)
        .bind(&p.profile_pic_url)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

/// Insert or refresh a thread's listing metadata without touching crawl state.
///
/// Every inbox listing re-upserts every thread it sees, and a listing carries none of
/// the ingester-owned columns. Assigning EXCLUDED straight across would therefore rewind
/// an in-flight backfill on every poll pass.
///
/// The text columns are held by `COALESCE(EXCLUDED.col, thread.col)`: NULL means "the
/// writer knows nothing about this", not "clear it".
///
/// COALESCE cannot save `backfill_done`, because false is a value and not an absence —
/// `COALESCE(false, true)` is false, and the crawl would be marked incomplete again. It
/// is an OR instead, which is monotonic: a thread that has been backfilled stays
/// backfilled no matter how often it is re-listed. Resetting it is an explicit act and
/// belongs to [`set_backfill`], the only function that can write it false.
pub async fn upsert_thread(pool: &PgPool, thread: &ThreadRecord) -> Result<(), sqlx::Error> {
    query(
        "
        INSERT INTO thread (
          account_id, thread_id, title, is_group, participant_ids, last_activity_at,
          watermark_item_id, backfill_cursor, backfill_done, last_polled_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        ON CONFLICT (account_id, thread_id) DO UPDATE SET
          title             = COALESCE(EXCLUDED.title, thread.title),
          is_group          = EXCLUDED.is_group,
          participant_ids   = EXCLUDED.participant_ids,
          last_activity_at  = COALESCE(EXCLUDED.last_activity_at, thread.last_activity_at),
          watermark_item_id = COALESCE(EXCLUDED.watermark_item_id, thread.watermark_item_id),
          backfill_cursor   = COALESCE(EXCLUDED.backfill_cursor, thread.backfill_cursor),
          backfill_done     = EXCLUDED.backfill_done OR thread.backfill_done,
          last_polled_at    = COALESCE(EXCLUDED.last_polled_at, thread.last_polled_at)
        ",
    )
    .bind(&thread.account_id)
    .bind(&thread.thread_id)
    .bind(&thread.title)
    .bind(thread.is_group)
    .bind(&thread.participant_ids)
    .bind(thread.last_activity_at)
    // A `ThreadRecord` is a listing and carries no crawl state: these are its absences.
    .bind(None::<String>)
    .bind(None::<String>)
    .bind(false)
    .bind(None::<DateTime<Utc>>)
    .execute(pool)
    .await?;
    Ok(())
}

/// Store a page of messages, returning how many rows were genuinely new.
///
/// Insert-only: a stored item is never rewritten. Instagram hands the same item back on
/// every overlapping page, and the archive's job is to hold what was sent, not to track
/// what the current payload happens to say about it.
///
/// The returned count is not `messages.len()` — it is what the database actually
/// inserted, and it is how a backfill loop learns that a page it just fetched was
/// already stored and the crawl has met itself.
///
/// One statement per chunk rather than one per message: a whole page goes out as a
/// single multi-row VALUES.
///
/// A missing thread fails with a foreign key violation, deliberately: a message with
/// nowhere to hang is a bug in the caller's write ordering, not a row to drop quietly.
pub async fn insert_messages(
    pool: &PgPool,
    messages: &[MessageRecord],
) -> Result<u64, sqlx::Error> {
    let mut total = 0;
    for chunk in messages.chunks(INSERT_CHUNK) {
        total += insert_message_chunk(pool, chunk).await?;
    }
    Ok(total)
}

async fn insert_message_chunk(
    pool: &PgPool,
    messages: &[MessageRecord],
) -> Result<u64, sqlx::Error> {
    // The builder only ever writes `$n` placeholders into the SQL; every value is bound.
    let mut sql: QueryBuilder<Postgres> =
        QueryBuilder::new("WITH new_rows AS ( INSERT INTO message (");
    sql.push(stored_message_columns!()).push(") ");
    sql.push_values(messages, message_values);
    sql.push(
        " ON CONFLICT (account_id, thread_id, item_id) DO NOTHING RETURNING 1 ) \
         SELECT count(*) FROM new_rows",
    );
    let inserted: i64 = sql.build_query_scalar().fetch_one(pool).await?;
    Ok(inserted as u64)
}

/// One message's columns, in `stored_message_columns!` order.
fn message_values(mut row: Separated<'_, Postgres, &'static str>, m: &MessageRecord) {
    row.push_bind(&m.account_id)
        .push_bind(&m.thread_id)
        .push_bind(&m.item_id)
        .push_bind(m.ts)
        .push_bind(&m.sender_id)
        .push_bind(&m.item_type)
        .push_bind(&m.text)
        // Derived in `normalize`, never in SQL: there is one implementation of which
        // payload key holds a share's author, and it is the one with the captures.
        .push_bind(&m.share_kind)
        .push_bind(&m.share_owner)
        .push_bind(&m.share_caption)
        .push_bind(&m.share_permalink)
        .push_bind(&m.reply_to_item_id)
        // `Json` encodes once, as jsonb. Encoding to a string first stored every payload
        // as a JSON string — the bug slack-sync shipped, invisible because the decode
        // path was symmetric.
        .push_bind(Json(&m.reactions))
        .push_bind(Json(&m.raw));
}

/// Record where the backfill crawl of one thread stands, and that it had its turn.
///
/// Both columns are assigned outright rather than COALESCEd: this is the loop that owns
/// them, and it has to be able to say "no cursor left" and "start again" as plainly as
/// it says "here is the next page".
pub async fn set_backfill(
    pool: &PgPool,
    account_id: &str,
    thread_id: &str,
    cursor: Option<&str>,
    done: bool,
) -> Result<(), sqlx::Error> {
    query(
        "
        UPDATE thread SET backfill_cursor = $3, backfill_done = $4, backfill_at = now()
         WHERE account_id = $1 AND thread_id = $2
        ",
    )
    .bind(account_id)
    .bind(thread_id)
    .bind(cursor)
    .bind(done)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record the newest item stored for a thread.
///
/// The poll loop compares Instagram's `last_activity_at` against this to decide whether
/// a thread is worth opening at all, which is what keeps a pass at a handful of requests
/// rather than one per thread.
pub async fn set_watermark(
    pool: &PgPool,
    account_id: &str,
    thread_id: &str,
    item_id: &str,
) -> Result<(), sqlx::Error> {
    query("UPDATE thread SET watermark_item_id = $3 WHERE account_id = $1 AND thread_id = $2")
        .bind(account_id)
        .bind(thread_id)
        .bind(item_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Stamp a thread as visited. This is also what spreads the backfill crawl: the next
/// [`threads_needing_backfill`] puts a freshly stamped thread last.
pub async fn mark_polled(
    pool: &PgPool,
    account_id: &str,
    thread_id: &str,
) -> Result<(), sqlx::Error> {
    query("UPDATE thread SET last_polled_at = now() WHERE account_id = $1 AND thread_id = $2")
        .bind(account_id)
        .bind(thread_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Record that a loop ran, with or without a failure.
///
/// The error columns are written on every call, not COALESCEd: a successful run clears
/// whatever the last failure left behind. A stale error that never clears makes the
/// status tool report an outage that ended hours ago, which is worse than reporting
/// nothing.
pub async fn record_sync(
    pool: &PgPool,
    account_id: &str,
    loop_name: &str,
    thread_id: Option<&str>,
    error: Option<&str>,
) -> Result<(), sqlx::Error> {
    query(
        "
        INSERT INTO sync_state (
          account_id, loop, current_thread_id, last_run_at, last_error, last_error_at
        )
        VALUES ($1, $2, $3, now(), $4, CASE WHEN $4::text IS NULL THEN NULL ELSE now() END)
        ON CONFLICT (account_id, loop) DO UPDATE SET
          current_thread_id = EXCLUDED.current_thread_id,
          last_run_at       = now(),
          last_error        = EXCLUDED.last_error,
          last_error_at     = EXCLUDED.last_error_at
        ",
    )
    .bind(account_id)
    .bind(loop_name)
    .bind(thread_id)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record a dead session. [`upsert_account`] leaves these columns alone, so the error
/// stands until a pass authenticates again and [`clear_auth_error`] lifts it.
pub async fn record_auth_error(
    pool: &PgPool,
    account_id: &str,
    error: &str,
) -> Result<(), sqlx::Error> {
    query(
        "
        UPDATE account
           SET last_auth_error    = $2,
               last_auth_error_at = now(),
               updated_at         = now()
         WHERE id = $1
        ",
    )
    .bind(account_id)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

/// Lift the dead-session flag, because a pass just authenticated.
///
/// Same reasoning as [`record_sync`]: an error that never clears makes the status tool
/// report an outage that has ended. The session does not have to be re-minted by hand
/// to recover — Instagram can simply stop rejecting it — and nothing else resets this.
pub async fn clear_auth_error(pool: &PgPool, account_id: &str) -> Result<(), sqlx::Error> {
    query(
        "
        UPDATE account
           SET last_auth_error    = NULL,
               last_auth_error_at = NULL,
               updated_at         = now()
         WHERE id = $1
           AND last_auth_error IS NOT NULL
        ",
    )
    .bind(account_id)
    .execute(pool)
    .await?;
    Ok(())
}

fn message_from_row(row: &PgRow) -> Result<MessageRecord, sqlx::Error> {
    Ok(MessageRecord {
        account_id: row.try_get("account_id")?,
        thread_id: row.try_get("thread_id")?,
        item_id: row.try_get("item_id")?,
        ts: row.try_get("ts")?,
        sender_id: row.try_get("sender_id")?,
        item_type: row.try_get("item_type")?,
        text: row.try_get("text")?,
        // Not selected here, as in Python, where the missing column read as false.
        edited: false,
        share_kind: row.try_get("share_kind")?,
        share_owner: row.try_get("share_owner")?,
        share_caption: row.try_get("share_caption")?,
        share_permalink: row.try_get("share_permalink")?,
        reply_to_item_id: row.try_get("reply_to_item_id")?,
        // Deliberately strict: a jsonb that holds a string fails to decode here rather
        // than being quietly unwrapped, which is what hid slack-sync's double encoding.
        reactions: row
            .try_get::<Json<Vec<serde_json::Value>>, _>("reactions")?
            .0,
        raw: row.try_get::<Json<serde_json::Value>, _>("raw")?.0,
    })
}

/// The latest item stored for a thread, or `None` if it holds nothing yet.
///
/// Ordered by `(ts, item_id)` like the index: Instagram ids sort by time in practice but
/// that is an observation, not a contract, and two items can share a timestamp.
pub async fn newest_message(
    pool: &PgPool,
    account_id: &str,
    thread_id: &str,
) -> Result<Option<MessageRecord>, sqlx::Error> {
    let row = query(concat!(
        "SELECT ",
        stored_message_columns!(),
        " FROM message
         WHERE account_id = $1 AND thread_id = $2
         ORDER BY ts DESC, item_id DESC
         LIMIT 1"
    ))
    .bind(account_id)
    .bind(thread_id)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(message_from_row).transpose()
}

/// Threads still owing history, the one whose last backfill turn is oldest first.
///
/// Round-robin on the backfill's own stamp: ordering by the poll's visit fixated on
/// quiet threads, which the poll never stamps, so one of them took every turn while the
/// busy ones never got one. A thread never backfilled sorts first, then the least
/// recently polled; NULLS FIRST is spelled out because that is the oldest there is.
pub async fn threads_needing_backfill(
    pool: &PgPool,
    account_id: &str,
    limit: i64,
) -> Result<Vec<StoredThread>, sqlx::Error> {
    query_as(concat!(
        "SELECT ",
        thread_columns!(),
        " FROM thread
         WHERE account_id = $1 AND NOT backfill_done
         ORDER BY backfill_at ASC NULLS FIRST, last_polled_at ASC NULLS FIRST, thread_id
         LIMIT $2"
    ))
    .bind(account_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

pub async fn get_thread(
    pool: &PgPool,
    account_id: &str,
    thread_id: &str,
) -> Result<Option<StoredThread>, sqlx::Error> {
    query_as(concat!(
        "SELECT ",
        thread_columns!(),
        " FROM thread WHERE account_id = $1 AND thread_id = $2"
    ))
    .bind(account_id)
    .bind(thread_id)
    .fetch_optional(pool)
    .await
}

/// The id of the oldest message stored for a thread: where backfill resumes.
pub async fn oldest_item_id(
    pool: &PgPool,
    account_id: &str,
    thread_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    query_scalar(
        "
        SELECT item_id FROM message
         WHERE account_id = $1 AND thread_id = $2
         ORDER BY ts ASC, item_id ASC
         LIMIT 1
        ",
    )
    .bind(account_id)
    .bind(thread_id)
    .fetch_optional(pool)
    .await
}

/// Whether a re-read is worth a write.
///
/// [`record_revision`] is safe to call unconditionally and returns false when nothing
/// changed — but it takes a connection and a row lock to find that out, and in steady
/// state the answer is false for every message in every window on every cycle. The
/// loops already hold the stored text, so the cheap comparison happens there and the
/// database is touched only when something actually moved.
pub fn text_differs(stored: Option<&str>, observed: Option<&str>) -> bool {
    !same_text(stored, observed)
}

/// NULL and '' are the same absence of text, and neither is an edit.
///
/// Most Instagram items are shares, reactions or media with no text at all. Treating
/// NULL as different from '' would score every one of them as edited on every poll.
fn same_text(a: Option<&str>, b: Option<&str>) -> bool {
    a.unwrap_or("") == b.unwrap_or("")
}

/// What the archive holds for one thread since `since`: item_id -> text.
///
/// Deleted rows are included, so a message that comes back is recognisable as one we
/// already had rather than looking like one we never held.
pub async fn stored_window(
    pool: &PgPool,
    account_id: &str,
    thread_id: &str,
    since: DateTime<Utc>,
) -> Result<HashMap<String, String>, sqlx::Error> {
    let rows: Vec<(String, String)> = query_as(
        "
        SELECT item_id, text FROM message
         WHERE account_id = $1 AND thread_id = $2 AND ts >= $3
        ",
    )
    .bind(account_id)
    .bind(thread_id)
    .bind(since)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Record that a stored message now reads differently, and update it. Idempotent.
///
/// Deliberately separate from [`insert_messages`] rather than turning that into an
/// upsert. Its return value is the count of rows genuinely inserted, and the backfill
/// reads that number to learn that a page it just fetched was already stored; an upsert
/// would report updates too and the crawl would stop recognising itself.
///
/// One transaction over a locked message row: `seq` is allocated as max+1, and a crash
/// between the revision insert and the text update would leave the log contradicting
/// the message it describes.
///
/// `message.text` is NOT NULL, so a `None` against non-empty stored text fails the
/// final UPDATE and rolls the whole call back, exactly as in Python.
pub async fn record_revision(
    pool: &PgPool,
    account_id: &str,
    thread_id: &str,
    item_id: &str,
    text: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let current: Option<String> = query_scalar(
        "
        SELECT text FROM message
         WHERE account_id = $1 AND thread_id = $2 AND item_id = $3 FOR UPDATE
        ",
    )
    .bind(account_id)
    .bind(thread_id)
    .bind(item_id)
    .fetch_optional(&mut *tx)
    .await?;
    // A first sighting is a store, not an edit. Calling it one would invent a history
    // that never happened.
    let Some(current) = current else {
        return Ok(false);
    };
    if same_text(Some(&current), text) {
        return Ok(false);
    }

    let mut seq: i32 = query_scalar(
        "
        SELECT coalesce(max(seq), 0) FROM message_revision
         WHERE account_id = $1 AND thread_id = $2 AND item_id = $3
        ",
    )
    .bind(account_id)
    .bind(thread_id)
    .bind(item_id)
    .fetch_one(&mut *tx)
    .await?;
    if seq == 0 {
        query(
            "
            INSERT INTO message_revision
                   (account_id, thread_id, item_id, seq, text, first_seen_at, superseded_at)
            VALUES ($1, $2, $3, 1, $4::text, NULL, now())
            ",
        )
        .bind(account_id)
        .bind(thread_id)
        .bind(item_id)
        .bind(&current)
        .execute(&mut *tx)
        .await?;
        seq = 1;
    } else {
        query(
            "
            UPDATE message_revision SET superseded_at = now()
             WHERE account_id = $1 AND thread_id = $2 AND item_id = $3
               AND superseded_at IS NULL
            ",
        )
        .bind(account_id)
        .bind(thread_id)
        .bind(item_id)
        .execute(&mut *tx)
        .await?;
    }
    query(
        "
        INSERT INTO message_revision
               (account_id, thread_id, item_id, seq, text, first_seen_at, superseded_at)
        VALUES ($1, $2, $3, $4, $5::text, now(), NULL)
        ",
    )
    .bind(account_id)
    .bind(thread_id)
    .bind(item_id)
    .bind(seq + 1)
    .bind(text)
    .execute(&mut *tx)
    .await?;
    query(
        "
        UPDATE message SET text = $4::text
         WHERE account_id = $1 AND thread_id = $2 AND item_id = $3
        ",
    )
    .bind(account_id)
    .bind(thread_id)
    .bind(item_id)
    .bind(text)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// Still there: neither missing nor deleted. Returns how many rows changed.
///
/// Clearing `deleted_at` too is deliberate. If a message we called deleted comes back,
/// we were wrong, and withdrawing the claim beats keeping a tombstone the evidence no
/// longer supports.
pub async fn mark_present(
    pool: &PgPool,
    account_id: &str,
    thread_id: &str,
    item_ids: &[String],
) -> Result<u64, sqlx::Error> {
    if item_ids.is_empty() {
        return Ok(0);
    }
    let result = query(
        "
        UPDATE message SET missing_since = NULL, deleted_at = NULL
         WHERE account_id = $1 AND thread_id = $2 AND item_id = ANY($3::text[])
           AND (missing_since IS NOT NULL OR deleted_at IS NOT NULL)
        ",
    )
    .bind(account_id)
    .bind(thread_id)
    .bind(item_ids)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Two strikes: the first clean pass that misses a message stamps `missing_since`, the
/// second promotes it to `deleted_at`.
///
/// An absence is not evidence on its own — a truncated page looks identical — and
/// `deleted_at` hides the message from every default view, so a false positive quietly
/// removes real history.
///
/// Returns how many were promoted on this call, counted from RETURNING rather than the
/// rows-affected count, which also counts first strikes.
pub async fn mark_absent(
    pool: &PgPool,
    account_id: &str,
    thread_id: &str,
    item_ids: &[String],
) -> Result<u64, sqlx::Error> {
    if item_ids.is_empty() {
        return Ok(0);
    }
    let deleted: Vec<Option<DateTime<Utc>>> = query_scalar(
        "
        UPDATE message
           SET deleted_at = CASE WHEN missing_since IS NOT NULL THEN now() ELSE deleted_at END,
               missing_since = coalesce(missing_since, now())
         WHERE account_id = $1 AND thread_id = $2 AND item_id = ANY($3::text[])
           AND deleted_at IS NULL
        RETURNING deleted_at
        ",
    )
    .bind(account_id)
    .bind(thread_id)
    .bind(item_ids)
    .fetch_all(pool)
    .await?;
    Ok(deleted.iter().filter(|at| at.is_some()).count() as u64)
}

/// Drop a one-strike absence once the message has aged out of the sweep window.
///
/// Nothing will look at it again, so corroboration is impossible. Promoting instead
/// would turn "we stopped being able to check" into "it was deleted".
pub async fn release_stranded(
    pool: &PgPool,
    account_id: &str,
    thread_id: &str,
    before: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let result = query(
        "
        UPDATE message SET missing_since = NULL
         WHERE account_id = $1 AND thread_id = $2 AND ts < $3
           AND missing_since IS NOT NULL AND deleted_at IS NULL
        ",
    )
    .bind(account_id)
    .bind(thread_id)
    .bind(before)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Narrow deletion candidates to those inside the range a page actually covered.
///
/// A full page is the newest N items, which usually falls short of the sweep window.
/// Everything older than the oldest item it returned is unvisited, and unvisited is not
/// evidence of anything.
pub async fn held_since(
    pool: &PgPool,
    account_id: &str,
    thread_id: &str,
    floor: DateTime<Utc>,
    candidates: &HashSet<String>,
) -> Result<HashSet<String>, sqlx::Error> {
    if candidates.is_empty() {
        return Ok(HashSet::new());
    }
    let mut sorted: Vec<&str> = candidates.iter().map(String::as_str).collect();
    sorted.sort();
    let rows: Vec<String> = query_scalar(
        "
        SELECT item_id FROM message
         WHERE account_id = $1 AND thread_id = $2 AND ts >= $3
           AND item_id = ANY($4::text[])
        ",
    )
    .bind(account_id)
    .bind(thread_id)
    .bind(floor)
    .bind(sorted)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// The account's saved cookie jar, as the JSON array it was saved as.
pub async fn load_session(pool: &PgPool, account_id: &str) -> Result<Option<String>, sqlx::Error> {
    query_scalar("SELECT cookies::text FROM ingest_session WHERE account_id = $1")
        .bind(account_id)
        .fetch_optional(pool)
        .await
}

/// Save the jar, replacing the last one: only the newest rotation is a session.
pub async fn save_session(
    pool: &PgPool,
    account_id: &str,
    cookies: &str,
) -> Result<(), sqlx::Error> {
    query(
        "
        INSERT INTO ingest_session (account_id, cookies, updated_at)
        VALUES ($1, $2::jsonb, now())
        ON CONFLICT (account_id) DO UPDATE SET
          cookies    = EXCLUDED.cookies,
          updated_at = now()
        ",
    )
    .bind(account_id)
    .bind(cookies)
    .execute(pool)
    .await?;
    Ok(())
}

/// Backfill requests already spent on `day`, across restarts.
pub async fn backfill_spent(
    pool: &PgPool,
    account_id: &str,
    day: NaiveDate,
) -> Result<i64, sqlx::Error> {
    let spent: Option<i64> = query_scalar(
        "SELECT budget_spent FROM sync_state
          WHERE account_id = $1 AND loop = 'backfill' AND budget_day = $2",
    )
    .bind(account_id)
    .bind(day)
    .fetch_optional(pool)
    .await?;
    Ok(spent.unwrap_or(0))
}

/// Add `requests` to `day`'s count. A different day than the stored one starts from zero.
pub async fn spend_backfill(
    pool: &PgPool,
    account_id: &str,
    day: NaiveDate,
    requests: i64,
) -> Result<(), sqlx::Error> {
    query(
        "
        INSERT INTO sync_state (account_id, loop, budget_day, budget_spent)
        VALUES ($1, 'backfill', $2, $3)
        ON CONFLICT (account_id, loop) DO UPDATE SET
          budget_spent = CASE WHEN sync_state.budget_day = EXCLUDED.budget_day
                              THEN sync_state.budget_spent + EXCLUDED.budget_spent
                              ELSE EXCLUDED.budget_spent END,
          budget_day   = EXCLUDED.budget_day
        ",
    )
    .bind(account_id)
    .bind(day)
    .bind(requests)
    .execute(pool)
    .await?;
    Ok(())
}
