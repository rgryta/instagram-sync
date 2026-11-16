//! What the `status` tool reads: how much is stored, how much history is still owed,
//! and whether ingest is stuck.

use sqlx::postgres::PgPool;
use sqlx::{Row, query};

use crate::models::{AccountStatus, SyncLoopStatus};

/// Per account: how much is stored, how much history is still owed, session health.
///
/// Scalar subqueries rather than joins: counting messages and participants in the same
/// join would multiply the two cardinalities together and report both wrong.
pub async fn account_status(pool: &PgPool) -> Result<Vec<AccountStatus>, sqlx::Error> {
    let rows = query(concat!(
        "
        SELECT a.id AS account_id, a.username, a.enabled,
               a.last_auth_error, a.last_auth_error_at,
               (SELECT count(*) FROM thread t WHERE t.account_id = a.id) AS threads,
               (SELECT count(*) FROM thread t
                 WHERE t.account_id = a.id AND NOT t.backfill_done) AS threads_pending_backfill,
               (SELECT count(*) FROM message m
                 WHERE m.account_id = a.id AND ",
        is_a_message!(),
        ") AS messages,
               -- Reported rather than hidden, and reported apart: a total that shrinks
               -- with no explanation reads as data loss, and the two reasons a row is
               -- not a message mean different things to whoever is reading. The second
               -- is an archive gap and the more interesting of the two.
               (SELECT count(*) FROM message m
                 WHERE m.account_id = a.id AND ",
        is_system!(),
        ") AS system_events,
               (SELECT count(*) FROM message m
                 WHERE m.account_id = a.id AND ",
        is_unreadable!(),
        ") AS unreadable,
               (SELECT count(*) FROM message m
                 WHERE m.account_id = a.id AND m.share_kind IS NOT NULL) AS shares,
               (SELECT count(*) FROM participant p WHERE p.account_id = a.id) AS participants,
               (SELECT min(m.ts) FROM message m
                 WHERE m.account_id = a.id AND ",
        is_a_message!(),
        ") AS oldest_message_at,
               (SELECT max(m.ts) FROM message m
                 WHERE m.account_id = a.id AND ",
        is_a_message!(),
        ") AS newest_message_at
          FROM account a
         ORDER BY a.id"
    ))
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| {
            Ok(AccountStatus {
                account_id: row.try_get("account_id")?,
                username: row.try_get("username")?,
                enabled: row.try_get("enabled")?,
                last_auth_error: row.try_get("last_auth_error")?,
                last_auth_error_at: row.try_get("last_auth_error_at")?,
                threads: row.try_get("threads")?,
                threads_pending_backfill: row.try_get("threads_pending_backfill")?,
                messages: row.try_get("messages")?,
                system_events: row.try_get("system_events")?,
                unreadable: row.try_get("unreadable")?,
                shares: row.try_get("shares")?,
                participants: row.try_get("participants")?,
                oldest_message_at: row.try_get("oldest_message_at")?,
                newest_message_at: row.try_get("newest_message_at")?,
            })
        })
        .collect()
}

/// One row per ingest loop, with whatever it last failed at.
pub async fn sync_loops(pool: &PgPool) -> Result<Vec<SyncLoopStatus>, sqlx::Error> {
    let rows = query(
        "
        SELECT account_id, loop, current_thread_id, last_run_at, last_error, last_error_at
          FROM sync_state
         ORDER BY account_id, loop
        ",
    )
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| {
            Ok(SyncLoopStatus {
                account_id: row.try_get("account_id")?,
                loop_name: row.try_get("loop")?,
                current_thread_id: row.try_get("current_thread_id")?,
                last_run_at: row.try_get("last_run_at")?,
                last_error: row.try_get("last_error")?,
                last_error_at: row.try_get("last_error_at")?,
            })
        })
        .collect()
}
