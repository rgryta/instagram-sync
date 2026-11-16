//! A port of `ingest/backfill.py`: reach the beginning of every thread, slowly.
//!
//! It yields: every request passes [`BACKFILL_RESERVE`] to the token bucket, so a poll
//! pass, which passes none, takes the bucket ahead of it. And it is budgeted, not merely
//! rate-limited: a pass spends at most `request_budget` pages and the day at most
//! `backfill_daily_requests`, counted in `sync_state`.
//!
//! A turn resumes just older than a message the archive holds: the thread's cursor when
//! it has one, else its oldest stored message. So a turn costs only the pages it gains.
//! (It used to re-walk from the newest page every time, and a thread `n` pages deep cost
//! `n` requests before it gained one.) A thread holding nothing starts at its newest page.
//! Setting the cursor to a message just after a hole makes the next turns walk down
//! through the hole; the re-read of what is already stored inserts nothing.
//!
//! Termination is a page that says it has nothing older behind it. A short page is not
//! the end: the server picks its own page size.

use std::collections::HashMap;

use instagram_core::store;
use serde_json::Value;
use sqlx::PgPool;

use crate::IngestError;
use crate::normalize;
use crate::source::Source;

/// Backfill asks the bucket to leave this much headroom, so a concurrent poll pass is
/// always served first. Below `burst`, or the reserve could never be satisfied.
pub const BACKFILL_RESERVE: f64 = 1.5;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BackfillTally {
    pub requests: u64,
    pub messages: u64,
    pub threads_finished: u64,
}

/// A resume point `older_than_message_id` honours. A REST-era numeric `item_id` would be
/// answered with an empty last page and finish the thread on the spot.
fn resumable(id: &str) -> bool {
    !id.is_empty() && !id.bytes().all(|b| b.is_ascii_digit())
}

/// A string or numeric id, the way Python's `str()` would print it.
fn id(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) if n.as_f64() != Some(0.0) => Some(n.to_string()),
        _ => None,
    }
}

/// Spend at most `request_budget` pages reaching further back.
pub async fn backfill_once(
    pool: &PgPool,
    source: &mut impl Source,
    account_id: &str,
    request_budget: i64,
) -> Result<BackfillTally, IngestError> {
    let mut tally = BackfillTally::default();
    if request_budget <= 0 {
        return Ok(tally);
    }
    let budget = request_budget as u64;

    // A conversation is addressed by its `thread_fbid`, which is not what the archive
    // keys on and is not stored. The inbox carries both and is cached for the pass, so
    // the map costs nothing on top of the poll. It also bounds this loop to the threads
    // the inbox currently lists.
    let handles: HashMap<String, String> = source
        .inbox()
        .await?
        .iter()
        .filter_map(|raw| Some((id(raw.get("thread_id"))?, id(raw.get("thread_fbid"))?)))
        .collect();

    // Round-robin on each thread's last turn, so the crawl spreads across the corpus.
    for thread in store::threads_needing_backfill(pool, account_id, 10).await? {
        if tally.requests >= budget {
            break;
        }
        let Some(handle) = handles.get(&thread.thread_id) else {
            tracing::debug!("no inbox entry for {}; cannot address it", thread.thread_id);
            // Stamped all the same, or it would hold the head of the queue for good.
            let cursor = thread.backfill_cursor.as_deref();
            store::set_backfill(pool, account_id, &thread.thread_id, cursor, false).await?;
            continue;
        };
        let pages = (budget - tally.requests) as usize;
        let from = match thread.backfill_cursor.filter(|c| resumable(c)) {
            Some(cursor) => Some(cursor),
            None => store::oldest_item_id(pool, account_id, &thread.thread_id)
                .await?
                .filter(|id| resumable(id)),
        };
        let payloads = match &from {
            Some(from) => {
                source
                    .older(handle, &thread.thread_id, from, pages, BACKFILL_RESERVE)
                    .await?
            }
            None => {
                source
                    .thread(handle, None, pages - 1, BACKFILL_RESERVE)
                    .await?
                    .0
            }
        };
        tally.requests += payloads.len() as u64;

        let mut oldest = from;
        for payload in &payloads {
            let messages = normalize::messages_from_thread(payload, account_id)?;
            // Oldest first, so the first of the last page is the furthest reached.
            if let Some(first) = messages.first() {
                oldest = Some(first.item_id.clone());
            }
            if !messages.is_empty() {
                tally.messages += store::insert_messages(pool, &messages).await?;
            }
        }

        // Persisted after the pages are committed, so an interrupted pass resumes at a
        // message it has already stored. Re-reading is free; skipping is invisible.
        let done = payloads
            .last()
            .is_some_and(|last| normalize::older_page_cursor(last).is_none());
        let cursor = (!done).then_some(oldest).flatten();
        store::set_backfill(pool, account_id, &thread.thread_id, cursor.as_deref(), done).await?;
        if done {
            tally.threads_finished += 1;
            tracing::info!("backfill reached the start of {}", thread.thread_id);
        }
    }
    Ok(tally)
}
