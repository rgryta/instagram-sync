//! A port of `ingest/backfill.py`: reach the beginning of every thread, slowly.
//!
//! It yields: every request passes [`BACKFILL_RESERVE`] to the token bucket, so a poll
//! pass, which passes none, takes the bucket ahead of it. And it is budgeted, not merely
//! rate-limited: a pass spends at most `request_budget` pages and the day at most
//! `backfill_daily_requests`, counted in `sync_state`.
//!
//! The walk only goes one way. A thread opens at its newest message, so each pass
//! re-walks the history it already holds before it reaches new ground, and asks for
//! `depth` pages where the last pass managed fewer. The re-walk stores nothing:
//! `insert_messages` counts only rows that were genuinely new.
//!
//! Termination is a page that says it has nothing older behind it. A short page is not
//! the end: the server picks its own page size.

use std::collections::HashMap;

use instagram_core::models::StoredThread;
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

/// How many pages deep the last pass got, from `backfill_cursor`, which holds a page
/// count rather than one of Instagram's cursors. Anything else (the REST cursor this
/// column used to hold) starts again from the top.
fn depth(thread: &StoredThread) -> usize {
    thread
        .backfill_cursor
        .as_deref()
        .and_then(|raw| raw.trim().parse::<i64>().ok())
        .map_or(0, |depth| depth.max(0) as usize)
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

    // Oldest-polled first, so the crawl spreads across the corpus.
    for thread in store::threads_needing_backfill(pool, account_id, 10).await? {
        let Some(handle) = handles.get(&thread.thread_id) else {
            tracing::debug!("no inbox entry for {}; cannot address it", thread.thread_id);
            continue;
        };
        let reached = depth(&thread);
        // Go deeper than last time, or this pass re-reads exactly what that one did.
        let (payloads, _met) = source
            .thread(handle, None, reached + budget as usize, BACKFILL_RESERVE)
            .await?;
        tally.requests += payloads.len() as u64;

        for payload in &payloads {
            let messages = normalize::messages_from_thread(payload, account_id)?;
            if !messages.is_empty() {
                tally.messages += store::insert_messages(pool, &messages).await?;
            }
        }

        // Persisted after the pages are committed, so an interrupted pass resumes at a
        // depth it has already stored. Re-reading is free; skipping is invisible.
        let done = payloads
            .last()
            .is_some_and(|last| normalize::older_page_cursor(last).is_none());
        let depth = reached.max(payloads.len()).to_string();
        store::set_backfill(
            pool,
            account_id,
            &thread.thread_id,
            (!done).then_some(depth.as_str()),
            done,
        )
        .await?;
        if done {
            tally.threads_finished += 1;
            tracing::info!("backfill reached the start of {}", thread.thread_id);
        }

        if tally.requests >= budget {
            break;
        }
    }
    Ok(tally)
}
