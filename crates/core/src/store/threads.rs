//! The thread listing.

use chrono::{DateTime, Utc};
use sqlx::postgres::PgPool;
use sqlx::{Row, query};

use crate::models::ThreadSummary;

#[derive(Debug, Clone, Default)]
pub struct ThreadPage {
    pub threads: Vec<ThreadSummary>,
    pub total_matches: i64,
}

/// Conversations, most recently active first.
///
/// `since` narrows to threads that moved after that instant *and* counts only what
/// arrived after it, which is what makes "what did I miss" one call rather than one per
/// thread. Without it the count is the whole stored history of the thread.
pub async fn list_threads(
    pool: &PgPool,
    account: Option<&str>,
    since: Option<DateTime<Utc>>,
    is_group: Option<bool>,
    limit: i64,
) -> Result<ThreadPage, sqlx::Error> {
    let rows = query(concat!(
        "
        SELECT t.account_id, t.thread_id, t.is_group, t.last_activity_at, t.backfill_done,
               (SELECT count(*) FROM message m
                 WHERE m.account_id = t.account_id AND m.thread_id = t.thread_id
                   AND ",
        is_a_message!(),
        "
                   AND ($2::timestamptz IS NULL OR m.ts > $2)) AS message_count, ",
        total_matches!(),
        "
          FROM thread t
         WHERE ($1::text IS NULL OR t.account_id = $1)
           AND ($2::timestamptz IS NULL OR t.last_activity_at > $2)
           AND ($3::boolean IS NULL OR t.is_group = $3)
         ORDER BY t.last_activity_at DESC NULLS LAST, t.thread_id
         LIMIT $4"
    ))
    .bind(account)
    .bind(since)
    .bind(is_group)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let total_matches = match rows.first() {
        Some(row) => row.try_get("total_matches")?,
        None => 0,
    };
    let threads = rows
        .iter()
        .map(|row| {
            Ok(ThreadSummary {
                account_id: row.try_get("account_id")?,
                thread_id: row.try_get("thread_id")?,
                is_group: row.try_get("is_group")?,
                last_activity_at: row.try_get("last_activity_at")?,
                backfill_done: row.try_get("backfill_done")?,
                message_count: row.try_get("message_count")?,
            })
        })
        .collect::<Result<_, sqlx::Error>>()?;
    Ok(ThreadPage {
        threads,
        total_matches,
    })
}
