//! A port of `ingest/poll.py`: keep the archive level with today.
//!
//! One pass walks the inbox, records every thread and participant it sees, and opens
//! only the threads whose activity has moved past what is already stored. A quiet inbox
//! costs the listing and nothing else.
//!
//! A pass reads back only as far as the newest message it already holds. GraphQL hands
//! back twenty messages at a time, so a thread that received more than twenty between
//! two passes does not fit in one page, and backfill only ever extends the oldest end.
//! So the pass pages back until it meets known ground, bounded by [`POLL_PAGES`].
//!
//! Anything older than the archive's oldest message is still backfill's job.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use chrono::{TimeDelta, Utc};
use instagram_core::models::{MessageRecord, ThreadRecord};
use instagram_core::store;
use serde_json::Value;
use sqlx::PgPool;

use crate::IngestError;
use crate::normalize::{self, NormalizeError};
use crate::source::Source;

/// How far back one pass will page a single thread when it has not yet met known
/// ground. A pass level with Instagram meets the watermark on the first page and never
/// spends these; the bound stops one neglected thread eating a whole pass.
pub const POLL_PAGES: usize = 8;

/// The tally for the log line and the status tool.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PollTally {
    pub threads_seen: u64,
    pub threads_opened: u64,
    pub messages: u64,
    pub revisions: u64,
    pub deletions: u64,
}

/// `last_activity_timestamp_ms`, which arrives as a string: string order puts a shorter
/// number first regardless of its value.
fn activity(raw: &Value) -> i64 {
    match raw.get("last_activity_timestamp_ms") {
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        _ => 0,
    }
}

pub async fn poll_once(
    pool: &PgPool,
    source: &mut impl Source,
    account_id: &str,
    thread_budget: usize,
    sweep_window_days: i64,
    head_only: bool,
) -> Result<PollTally, IngestError> {
    let mut tally = PollTally::default();
    let mut listing = if head_only {
        source.inbox_head().await?
    } else {
        source.inbox().await?
    };
    tally.threads_seen = listing.len() as u64;

    // Most recently active first, so a budget that runs out drops the threads least
    // likely to have anything new rather than an arbitrary slice.
    listing.sort_by_key(|raw| Reverse(activity(raw)));

    for raw in &listing {
        let thread = normalize::thread_from_payload(raw, account_id)?;
        store::upsert_participants(pool, &normalize::participants_from_thread(raw, account_id))
            .await?;
        store::upsert_thread(pool, &thread).await?;

        if tally.threads_opened >= thread_budget as u64 {
            continue;
        }
        if !has_moved(pool, &thread, account_id).await? {
            continue;
        }

        // Page back only until the newest message already stored comes round again.
        // With no watermark there is no known ground to stop at, and paging a brand new
        // thread to its beginning is backfill's job, not this pass's.
        let watermark = store::get_thread(pool, account_id, &thread.thread_id)
            .await?
            .and_then(|stored| stored.watermark_item_id);
        let pages = if watermark.is_some() { POLL_PAGES } else { 0 };
        let (payloads, _reached) = source
            .thread(
                &normalize::thread_handle(raw)?,
                watermark.as_deref(),
                pages,
                0.0,
            )
            .await?;
        tally.threads_opened += 1;
        let messages = merge(&payloads, account_id)?;
        if let Some(oldest_page) = payloads.last()
            && !messages.is_empty()
        {
            tally.messages += store::insert_messages(pool, &messages).await?;
            // After the insert and never inside it: the count `insert_messages` returns
            // is how the backfill learns a page was already stored.
            let since = Utc::now() - TimeDelta::days(sweep_window_days);
            let held = if sweep_window_days > 0 {
                store::stored_window(pool, account_id, &thread.thread_id, since).await?
            } else {
                HashMap::new()
            };
            tally.revisions +=
                reconcile(pool, &messages, &held, account_id, &thread.thread_id).await?;
            tally.deletions += reap(
                pool,
                &messages,
                &held,
                account_id,
                &thread.thread_id,
                since,
                // Whether older messages were left behind, read off the oldest page that
                // came back. The server picks the page size, so counting cannot tell.
                normalize::older_page_cursor(oldest_page).is_some(),
            )
            .await?;
            // The watermark advances only after the insert commits, so a crash between
            // the two re-reads a page rather than skipping one.
            let newest = messages
                .iter()
                .max_by(|a, b| (a.ts, &a.item_id).cmp(&(b.ts, &b.item_id)))
                .expect("messages is not empty");
            store::set_watermark(pool, account_id, &thread.thread_id, &newest.item_id).await?;
        }
        store::mark_polled(pool, account_id, &thread.thread_id).await?;
    }
    Ok(tally)
}

/// Every message across a thread's pages, oldest first, each one once.
///
/// The newest page is fetched by a different query than the older ones and both can
/// answer for the same window, so the first copy of an id wins.
fn merge(payloads: &[Value], account_id: &str) -> Result<Vec<MessageRecord>, NormalizeError> {
    let mut merged: HashMap<String, MessageRecord> = HashMap::new();
    for payload in payloads {
        for message in normalize::messages_from_thread(payload, account_id)? {
            merged.entry(message.item_id.clone()).or_insert(message);
        }
    }
    let mut messages: Vec<MessageRecord> = merged.into_values().collect();
    messages.sort_by(|a, b| (a.ts, &a.item_id).cmp(&(b.ts, &b.item_id)));
    Ok(messages)
}

/// Whether the listing says something happened since the newest stored message.
///
/// Compared against the stored message rather than a stored copy of Instagram's clock:
/// an edit or a reaction moves the clock without adding a message, and comparing to what
/// we hold self-heals if a pass was ever missed.
async fn has_moved(
    pool: &PgPool,
    thread: &ThreadRecord,
    account_id: &str,
) -> Result<bool, sqlx::Error> {
    let Some(last_activity) = thread.last_activity_at else {
        return Ok(true);
    };
    Ok(
        match store::newest_message(pool, account_id, &thread.thread_id).await? {
            None => true,
            Some(newest) => last_activity > newest.ts,
        },
    )
}

/// Record any message on this page whose stored text has changed. `held` is the window
/// the caller already read, so "nothing changed" costs a string compare.
async fn reconcile(
    pool: &PgPool,
    messages: &[MessageRecord],
    held: &HashMap<String, String>,
    account_id: &str,
    thread_id: &str,
) -> Result<u64, sqlx::Error> {
    let mut revised = 0;
    for message in messages {
        let Some(stored) = held.get(&message.item_id) else {
            continue;
        };
        if !store::text_differs(Some(stored), Some(&message.text)) {
            continue;
        }
        if store::record_revision(
            pool,
            account_id,
            thread_id,
            &message.item_id,
            Some(&message.text),
        )
        .await?
        {
            revised += 1;
        }
    }
    Ok(revised)
}

/// Mark what the archive holds in the range this page covered but did not return.
///
/// When `page_full` says older messages were left behind, the candidates are bounded by
/// the oldest item the page did return: everything above it was observed, everything
/// below it was never looked at, and unvisited is not evidence of anything. No extra
/// request is made for it.
async fn reap(
    pool: &PgPool,
    messages: &[MessageRecord],
    held: &HashMap<String, String>,
    account_id: &str,
    thread_id: &str,
    since: chrono::DateTime<Utc>,
    page_full: bool,
) -> Result<u64, sqlx::Error> {
    if held.is_empty() || messages.is_empty() {
        return Ok(0);
    }
    let seen: HashSet<&String> = messages.iter().map(|m| &m.item_id).collect();
    let mut present: Vec<String> = held
        .keys()
        .filter(|id| seen.contains(id))
        .cloned()
        .collect();
    present.sort();
    store::mark_present(pool, account_id, thread_id, &present).await?;
    let floor = if page_full {
        messages.iter().map(|m| m.ts).min().unwrap_or(since)
    } else {
        since
    };
    let unseen: HashSet<String> = held
        .keys()
        .filter(|id| !seen.contains(id))
        .cloned()
        .collect();
    let mut candidates: Vec<String> =
        store::held_since(pool, account_id, thread_id, floor, &unseen)
            .await?
            .into_iter()
            .collect();
    candidates.sort();
    let deleted = store::mark_absent(pool, account_id, thread_id, &candidates).await?;
    store::release_stranded(pool, account_id, thread_id, since).await?;
    Ok(deleted)
}
